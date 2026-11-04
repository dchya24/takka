//! Shared spinner widget — produces ratatui `Line`s for inline or TUI use.
//!
//! Unlike `ProgressState` (which tracks full progress with elapsed time),
//! this module provides simple styled spinner lines for quick rendering.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::progress::ProgressState;

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

/// Render a compact one-line status: spinner + message + indeterminate bar.
///
/// Used for inline transient progress where time-keeping isn't useful but
/// the bouncing bar gives the user motion feedback even when the spinner
/// glyph stalls (e.g. waiting on a long network call).
pub fn compact_progress_line(progress: &ProgressState, bar_width: usize) -> Line<'static> {
    if !progress.active {
        return Line::default();
    }

    let bar = progress.progress_bar(bar_width);
    let message = if progress.message.is_empty() {
        "Processing...".to_string()
    } else {
        progress.message.clone()
    };

    Line::from(vec![
        Span::styled(
            progress.spinner().to_string(),
            Style::default()
                .fg(Color::Rgb(52, 152, 219))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(message, Style::default().fg(Color::Rgb(180, 180, 180))),
        Span::raw("  "),
        Span::styled(bar, Style::default().fg(Color::Rgb(52, 152, 219))),
    ])
}

/// Render a "done" line after progress completes.
///
/// Returns: `✓ Done (2.3s)`
#[allow(dead_code)]
pub fn done_line(elapsed_ms: u128) -> Line<'static> {
    let elapsed_str = if elapsed_ms < 1000 {
        format!("{}ms", elapsed_ms)
    } else {
        format!("{}.{}s", elapsed_ms / 1000, (elapsed_ms % 1000) / 100)
    };

    Line::from(vec![
        Span::styled(
            "✓".to_string(),
            Style::default()
                .fg(Color::Rgb(46, 204, 113))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            "Done".to_string(),
            Style::default().fg(Color::Rgb(46, 204, 113)),
        ),
        Span::raw(" "),
        Span::styled(
            format!("({})", elapsed_str),
            Style::default().fg(Color::DarkGray),
        ),
    ])
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
    fn test_done_line() {
        let line = done_line(2300);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("Done"));
        assert!(text.contains("2.3s"));
    }

    #[test]
    fn test_error_line() {
        let line = error_line("something went wrong");
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("Error"));
        assert!(text.contains("something went wrong"));
    }

    #[test]
    fn test_compact_progress() {
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_message("Loading".to_string());

        let line = compact_progress_line(&progress, 10);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn compact_progress_width_is_stable_across_frames() {
        // The renderer overwrites the transient spinner line in place
        // (no Clear before repaint), so every frame MUST have the same
        // display width or stale characters would be left behind.
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_message("Thinking…".to_string());

        let first: String = compact_progress_line(&progress, 18)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        let first_len = first.chars().count();
        assert!(first_len > 0);

        for _ in 0..20 {
            progress.tick();
            let frame: String = compact_progress_line(&progress, 18)
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect();
            assert_eq!(
                frame.chars().count(),
                first_len,
                "spinner frame changed width — in-place overwrite would ghost text"
            );
        }
    }
}
