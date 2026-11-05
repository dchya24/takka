//! Shared spinner widget — produces ratatui `Line`s for inline or TUI use.
//!
//! Unlike `ProgressState` (which tracks full progress with elapsed time),
//! this module provides simple styled spinner lines for quick rendering.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::progress::{Phase, ProgressState};

/// Accent color per phase — the status line reads as "blue = waiting on the
/// model, green = output flowing, amber = a tool owns the run".
fn phase_accent(phase: &Phase) -> Color {
    match phase {
        Phase::Connecting => Color::Rgb(130, 170, 230),
        Phase::Thinking => Color::Rgb(52, 152, 219),
        Phase::Streaming => Color::Rgb(46, 204, 113),
        Phase::Tool => Color::Rgb(241, 196, 15),
        Phase::WaitingApproval => Color::Rgb(255, 183, 3),
    }
}

/// Render the live activity line shown while the agent works.
///
/// Replaces the old glyph + label + bouncing bar (whose motion carried no
/// information) with what the user actually wants to know: which phase we are
/// in, how long it has taken, how much output has arrived, and that the run
/// can be cancelled.
///
/// ```text
/// ⠹ thinking · 4.2s
/// ⠼ streaming · 342 tok · 12.4s
/// ⠴ run_command · 2.1s
/// ⠇ waiting for approval · ctrl+c to cancel
/// ```
pub fn status_line(progress: &ProgressState, show_cancel_hint: bool) -> Line<'static> {
    if !progress.active {
        return Line::default();
    }

    let accent = phase_accent(&progress.phase);
    let label = match (&progress.phase, &progress.tool_name) {
        (Phase::Tool, Some(name)) => name.clone(),
        (phase, _) => phase.label().to_string(),
    };

    let mut spans = vec![
        Span::styled(
            progress.spinner().to_string(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            label,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
    ];

    let dim = || Style::default().fg(Color::Rgb(140, 140, 155));

    // Token count first when streaming: it is the number that moves.
    if matches!(progress.phase, Phase::Streaming) {
        let tokens = progress.estimated_output_tokens();
        if tokens > 0 {
            spans.push(Span::styled(
                format!(" · {} tok", ProgressState::format_count(tokens)),
                dim(),
            ));
        }
    }

    // Clock: the running tool's own elapsed time (so a hang is visible)
    // versus the run's total for the model-side phases. Approvals are the
    // user's move — a countdown there is noise.
    if !matches!(progress.phase, Phase::WaitingApproval) {
        let clock = match progress.phase {
            Phase::Tool => progress
                .tool_elapsed_precise_str()
                .unwrap_or_else(|| progress.elapsed_precise_str()),
            _ => progress.elapsed_precise_str(),
        };
        spans.push(Span::styled(format!(" · {}", clock), dim()));
    }

    if show_cancel_hint {
        spans.push(Span::styled(" · ctrl+c to cancel", dim()));
    }

    Line::from(spans)
}

/// Render a spinner line with message and elapsed time.
///
/// Render a spinner line with message.
///
/// Returns a styled `Line` like: `⠙ Thinking...`
///
/// Elapsed time is intentionally not included — callers that want time
/// should render it separately, and most CLI consumers prefer a stable
/// width so the transient line redraws cleanly.
pub fn spinner_line(progress: &ProgressState) -> Line<'static> {
    if !progress.active {
        return Line::default();
    }

    Line::from(vec![
        Span::styled(
            progress.spinner().to_string(),
            Style::default()
                .fg(Color::Rgb(52, 152, 219))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            if progress.message.is_empty() {
                "Processing...".to_string()
            } else {
                progress.message.clone()
            },
            Style::default().fg(Color::Rgb(180, 180, 180)),
        ),
    ])
}

/// Render a progress bar line (determinate or indeterminate).
///
/// Returns a styled `Line` with the progress bar characters.
pub fn progress_bar_line(progress: &ProgressState, width: usize) -> Line<'static> {
    let bar = progress.progress_bar(width);

    let style = if progress.percentage.is_some() {
        Style::default().fg(Color::Rgb(46, 204, 113))
    } else {
        Style::default().fg(Color::Rgb(52, 152, 219))
    };

    Line::from(Span::styled(bar, style))
}

/// Render the end-of-run summary line.
///
/// Replaces the old bare `✓ Done (2.3s)` (+ a separate session-totals line):
/// one line carries the outcome, wall time, tool count and the tokens this run
/// produced, e.g.
///
/// ```text
/// ✓ done · 9.1s · 3 tools · ↑1.2k ↓342
/// ```
///
/// Token counts are estimates (~4 chars/token): the runtime protocol does not
/// surface provider usage to the CLI yet.
pub fn run_summary_line(
    elapsed_ms: u128,
    input_tokens: u32,
    output_tokens: u32,
    tool_calls: u32,
    measured: bool,
) -> Line<'static> {
    let elapsed_str = if elapsed_ms < 1000 {
        format!("{}ms", elapsed_ms)
    } else if elapsed_ms < 60_000 {
        format!("{}.{}s", elapsed_ms / 1000, (elapsed_ms % 1000) / 100)
    } else {
        format!(
            "{}m {:02}s",
            elapsed_ms / 60_000,
            (elapsed_ms % 60_000) / 1000
        )
    };

    let dim = || Style::default().fg(Color::Rgb(140, 140, 155));

    let mut spans = vec![
        Span::styled(
            "✓",
            Style::default()
                .fg(Color::Rgb(46, 204, 113))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " done",
            Style::default()
                .fg(Color::Rgb(46, 204, 113))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", elapsed_str), dim()),
    ];

    if tool_calls > 0 {
        spans.push(Span::styled(
            format!(
                " · {} tool{}",
                tool_calls,
                if tool_calls == 1 { "" } else { "s" }
            ),
            dim(),
        ));
    }

    // `~` marks an estimate (the provider reported no usage for this run).
    let (up, down) = if measured {
        ("\u{2191}", "\u{2193}")
    } else {
        ("~\u{2191}", "~\u{2193}")
    };
    spans.push(Span::styled(
        format!(
            " · {}{} {}{}",
            up,
            ProgressState::format_count(input_tokens),
            down,
            ProgressState::format_count(output_tokens)
        ),
        Style::default().fg(Color::Rgb(186, 85, 211)),
    ));

    Line::from(spans)
}

/// Render an error line.
///
/// Returns: `✗ Error: <message>`
#[allow(dead_code)]
pub fn error_line(message: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "✗".to_string(),
            Style::default()
                .fg(Color::Rgb(231, 76, 60))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("Error: {}", message),
            Style::default().fg(Color::Rgb(231, 76, 60)),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_line_active() {
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_message("Thinking...".to_string());

        let line = spinner_line(&progress);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn spinner_line_does_not_include_elapsed_time() {
        // Elapsed seconds were removed in favor of the bar in
        // `compact_progress_line`. The plain spinner line should be just
        // glyph + message.
        use std::thread::sleep;
        use std::time::Duration;
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_message("Thinking...".to_string());
        // Force at least 1s of elapsed time.
        sleep(Duration::from_millis(1100));
        let line = spinner_line(&progress);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!text.contains("("));
        assert!(!text.contains("s)"));
    }

    #[test]
    fn test_spinner_line_inactive() {
        let progress = ProgressState::new();
        let line = spinner_line(&progress);
        assert!(line.spans.is_empty());
    }

    #[test]
    fn run_summary_carries_time_tools_and_tokens() {
        let line = run_summary_line(9100, 1234, 342, 3, true);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("done"), "missing outcome: {text}");
        assert!(text.contains("9.1s"), "missing elapsed: {text}");
        assert!(text.contains("3 tools"), "missing tool count: {text}");
        assert!(
            text.contains("\u{2191}1.2k"),
            "missing input tokens: {text}"
        );
        assert!(
            text.contains("\u{2193}342"),
            "missing output tokens: {text}"
        );
        assert!(
            !text.contains('~'),
            "measured usage must not look estimated: {text}"
        );
    }

    #[test]
    fn run_summary_marks_estimates() {
        let line = run_summary_line(9100, 1234, 342, 0, false);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("~\u{2191}1.2k"),
            "estimate marker missing: {text}"
        );
        assert!(
            text.contains("~\u{2193}342"),
            "estimate marker missing: {text}"
        );
    }

    #[test]
    fn run_summary_omits_tools_when_none_ran() {
        let line = run_summary_line(850, 12, 40, 0, true);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!text.contains("tool"), "no tools ran: {text}");
        assert!(text.contains("850ms"), "sub-second runs use ms: {text}");
    }

    fn status_text(progress: &ProgressState, hint: bool) -> String {
        status_line(progress, hint)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn status_line_shows_phase_and_elapsed() {
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_phase(Phase::Thinking);
        let text = status_text(&progress, false);
        assert!(text.contains("thinking"), "phase label missing: {text}");
        assert!(text.contains('s'), "elapsed missing: {text}");
        assert!(!text.contains("ctrl+c"), "hint must be opt-in: {text}");
    }

    #[test]
    fn status_line_streaming_shows_token_estimate() {
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_phase(Phase::Streaming);
        progress.add_stream_chars(1_360); // ≈340 tokens
        let text = status_text(&progress, true);
        assert!(text.contains("streaming"), "{text}");
        assert!(text.contains("340 tok"), "token estimate missing: {text}");
        assert!(
            text.contains("ctrl+c to cancel"),
            "cancel hint missing: {text}"
        );
    }

    #[test]
    fn status_line_names_the_running_tool_and_hides_elapsed_on_approval() {
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_tool("run_command");
        let text = status_text(&progress, false);
        assert!(text.contains("run_command"), "{text}");

        progress.set_phase(Phase::WaitingApproval);
        let text = status_text(&progress, false);
        assert!(text.contains("waiting for approval"), "{text}");
        assert!(
            !text.contains(" · 0."),
            "approval wait should not tick time: {text}"
        );
    }

    /// The tool clock is separate from the run clock: a tool that has been
    /// running 0.0s inside a 3s run must show its own time.
    #[test]
    fn status_line_shows_the_per_tool_clock_while_a_tool_runs() {
        let mut progress = ProgressState::new();
        progress.start();
        // Pretend the run began a while ago.
        progress.start_time = Some(std::time::Instant::now() - std::time::Duration::from_secs(3));
        progress.set_tool("cargo test");
        let text = status_text(&progress, false);
        assert!(text.contains("cargo test"), "{text}");
        assert!(
            text.contains(" · 0.0s"),
            "tool clock should start at zero: {text}"
        );

        // Leaving the tool phase drops back to the run clock.
        progress.set_phase(Phase::Thinking);
        let text = status_text(&progress, false);
        assert!(text.contains(" · 3.0s"), "run clock expected: {text}");
    }

    #[test]
    fn re_marking_the_same_tool_keeps_its_clock() {
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_tool("read_file");
        let started = progress.tool_started;
        progress.set_tool("read_file");
        assert_eq!(progress.tool_started, started, "clock must not reset");
        progress.set_tool("write_file");
        assert_ne!(progress.tool_started, started, "new tool, new clock");
    }

    #[test]
    fn test_error_line() {
        let line = error_line("something went wrong");
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("Error"));
        assert!(text.contains("something went wrong"));
    }
}
