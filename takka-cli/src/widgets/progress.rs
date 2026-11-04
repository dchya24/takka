//! Shared progress state for animations.
//!
//! Used by both TUI (rendered as a widget) and CLI (rendered inline).

use std::time::Instant;

/// Spinner frames for loading animation
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Progress bar characters
const PROGRESS_FILLED: &str = "█";
const PROGRESS_EMPTY: &str = "░";

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
}

impl ProgressState {
    pub fn new() -> Self {
        Self {
            active: false,
            frame: 0,
            message: String::new(),
            start_time: None,
            percentage: None,
        }
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
