//! Shared progress state for animations.
//!
//! Used by both TUI (rendered as a widget) and CLI (rendered inline).

use std::time::Instant;

/// Spinner frames for loading animation
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Progress bar characters
const PROGRESS_FILLED: &str = "█";
const PROGRESS_EMPTY: &str = "░";

/// What the agent is doing right now. Drives the live status line shown
/// while a run is in flight (see `spinner::status_line`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Opening / reusing the runtime connection. Not used by the inline
    /// renderer today (the connection is established before its status line
    /// appears); kept for surfaces that can show it, e.g. the daemon path.
    Connecting,
    /// Request sent, waiting for the first token.
    Thinking,
    /// Tokens are arriving.
    Streaming,
    /// A tool is executing (name carried by `ProgressState::tool_name`).
    Tool,
    /// Parked on a confirmation / question prompt owned by the user.
    WaitingApproval,
}

impl Phase {
    /// Human label for phases that don't carry their own name.
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Connecting => "connecting…",
            Phase::Thinking => "thinking",
            Phase::Streaming => "streaming",
            Phase::Tool => "working",
            Phase::WaitingApproval => "waiting for approval",
        }
    }
}

/// Progress state for animations
#[derive(Clone, Debug)]
pub struct ProgressState {
    /// Is progress active
    pub active: bool,
    /// Current spinner frame index
    pub frame: usize,
    /// Progress message
    pub message: String,
    /// Start time
    pub start_time: Option<Instant>,
    /// Progress percentage (0-100) for determinate progress
    pub percentage: Option<u8>,
    /// Current activity, shown by the inline status line.
    pub phase: Phase,
    /// Name of the tool currently running (only meaningful for `Phase::Tool`).
    pub tool_name: Option<String>,
    /// When the running tool started — drives the per-tool clock in the status
    /// line, so a stuck tool is obvious without watching the whole run's timer.
    pub tool_started: Option<Instant>,
    /// Characters streamed by the model this run — token counts are estimated
    /// from this (see [`ProgressState::estimated_output_tokens`]).
    pub stream_chars: usize,
}

impl ProgressState {
    pub fn new() -> Self {
        Self {
            active: false,
            frame: 0,
            message: String::new(),
            start_time: None,
            percentage: None,
            phase: Phase::Thinking,
            tool_name: None,
            tool_started: None,
            stream_chars: 0,
        }
    }

    /// Mark the activity the status line should show.
    pub fn set_phase(&mut self, phase: Phase) {
        if phase != Phase::Tool {
            // Leaving the tool phase stops its clock.
            self.tool_started = None;
        }
        self.phase = phase;
    }

    /// Mark the tool that is currently executing and start its clock.
    ///
    /// Re-marking the same tool keeps the original start time so the clock
    /// doesn't reset on repeated `ToolCall`/`ToolStarted` events.
    pub fn set_tool(&mut self, name: impl Into<String>) {
        let name = name.into();
        if self.phase != Phase::Tool || self.tool_name.as_deref() != Some(name.as_str()) {
            self.tool_started = Some(Instant::now());
        }
        self.phase = Phase::Tool;
        self.tool_name = Some(name);
    }

    /// How long the current tool has been running, if one is.
    pub fn tool_elapsed_precise_str(&self) -> Option<String> {
        let started = self.tool_started?;
        Some(format_elapsed(started.elapsed()))
    }

    /// Count streamed characters (drives the live token estimate).
    pub fn add_stream_chars(&mut self, chars: usize) {
        self.stream_chars += chars;
    }

    /// Rough output-token estimate: ~4 characters per token.
    pub fn estimated_output_tokens(&self) -> u32 {
        (self.stream_chars / 4).min(u32::MAX as usize) as u32
    }

    /// Start progress animation
    pub fn start(&mut self) {
        self.active = true;
        self.frame = 0;
        self.start_time = Some(Instant::now());
        self.percentage = None;
    }

    /// Stop progress animation
    pub fn stop(&mut self) {
        self.active = false;
        self.start_time = None;
        self.percentage = None;
    }

    /// Set progress message
    pub fn set_message(&mut self, msg: String) {
        self.message = msg;
    }

    /// Set determinate progress
    #[allow(dead_code)]
    pub fn set_percentage(&mut self, pct: u8) {
        self.percentage = Some(pct.min(100));
    }

    /// Tick animation frame
    pub fn tick(&mut self) {
        if self.active {
            self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        }
    }

    /// Get current spinner character
    pub fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[self.frame]
    }

    /// Format tokens compactly: `842`, `1.2k`, `3.4m`.
    pub fn format_count(n: u32) -> String {
        if n >= 1_000_000 {
            format!("{:.1}m", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            format!("{}", n)
        }
    }

    /// Elapsed time with tenths of a second: `4.2s`, `1m 07s`.
    pub fn elapsed_precise_str(&self) -> String {
        match self.start_time {
            Some(start) => format_elapsed(start.elapsed()),
            None => "0.0s".to_string(),
        }
    }

    /// Get elapsed time string
    #[allow(dead_code)]
    pub fn elapsed_str(&self) -> String {
        if let Some(start) = self.start_time {
            let elapsed = start.elapsed();
            let secs = elapsed.as_secs();
            if secs < 60 {
                format!("{}s", secs)
            } else {
                format!("{}m {}s", secs / 60, secs % 60)
            }
        } else {
            String::new()
        }
    }

    /// Render progress bar string (for determinate progress)
    pub fn progress_bar(&self, width: usize) -> String {
        if let Some(pct) = self.percentage {
            let filled = (width as f32 * pct as f32 / 100.0) as usize;
            let empty = width.saturating_sub(filled);
            format!(
                "{}{}",
                PROGRESS_FILLED.repeat(filled),
                PROGRESS_EMPTY.repeat(empty)
            )
        } else {
            // Indeterminate: bouncing animation
            let pos = self.frame % (width * 2);
            let actual_pos = if pos >= width {
                width * 2 - pos - 1
            } else {
                pos
            };

            let mut bar = PROGRESS_EMPTY.repeat(width);
            if actual_pos < width {
                let bytes_per_char = PROGRESS_EMPTY.len();
                let start = actual_pos * bytes_per_char;
                let end = start + bytes_per_char;
                if end <= bar.len() {
                    bar.replace_range(start..end, PROGRESS_FILLED);
                }
            }
            bar
        }
    }

    /// Get full progress display string (plain text, for non-widget usage)
    #[allow(dead_code)]
    pub fn display(&self) -> String {
        if !self.active {
            return String::new();
        }

        let spinner = self.spinner();
        let elapsed = self.elapsed_str();
        let msg = if self.message.is_empty() {
            "Processing..."
        } else {
            &self.message
        };

        if let Some(pct) = self.percentage {
            format!("{} {} [{}] {}%", spinner, msg, self.progress_bar(20), pct)
        } else if elapsed.is_empty() {
            format!("{} {}", spinner, msg)
        } else {
            format!("{} {} ({})", spinner, msg, elapsed)
        }
    }
}

/// `4.2s` under a minute, `1m 07s` above it.
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{}.{}s", secs, elapsed.subsec_millis() / 100)
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

impl Default for ProgressState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_cycle() {
        let mut progress = ProgressState::new();
        progress.start();

        let first = progress.spinner();
        progress.tick();
        let second = progress.spinner();

        assert_ne!(first, second);
    }

    #[test]
    fn test_progress_bar_determinate() {
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_percentage(50);

        let bar = progress.progress_bar(10);
        assert!(bar.contains(PROGRESS_FILLED));
        assert!(bar.contains(PROGRESS_EMPTY));
    }

    #[test]
    fn test_display_message() {
        let mut progress = ProgressState::new();
        progress.start();
        progress.set_message("Loading...".to_string());

        let display = progress.display();
        assert!(display.contains("Loading..."));
    }

    #[test]
    fn test_progress_bar_indeterminate() {
        let mut progress = ProgressState::new();
        progress.start();

        let bar = progress.progress_bar(20);
        // Should contain exactly one filled block
        assert!(bar.contains(PROGRESS_FILLED));
    }
}
