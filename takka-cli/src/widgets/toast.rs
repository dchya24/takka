//! Toast notification system for inline CLI mode.
//!
//! Provides temporary notifications that auto-dismiss after a duration.
//! Used for feedback on user actions (success, error, warning, info).

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::time::{Duration, Instant};

/// Toast notification level
#[derive(Clone, Debug, PartialEq)]
pub enum ToastLevel {
    /// Informational message
    Info,
    /// Success message
    Success,
    /// Warning message
    Warning,
    /// Error message
    Error,
}

impl ToastLevel {
    /// Get icon for toast level
    pub fn icon(&self) -> &'static str {
        match self {
            ToastLevel::Info => "ℹ️",
            ToastLevel::Success => "✅",
            ToastLevel::Warning => "⚠️",
            ToastLevel::Error => "❌",
        }
    }

    /// Get color for toast level
    pub fn color(&self) -> Color {
        match self {
            ToastLevel::Info => Color::Rgb(52, 152, 219),
            ToastLevel::Success => Color::Rgb(46, 204, 113),
            ToastLevel::Warning => Color::Rgb(241, 196, 15),
            ToastLevel::Error => Color::Rgb(231, 76, 60),
        }
    }
}

/// Toast notification
#[derive(Clone, Debug)]
pub struct Toast {
    /// Toast message
    pub message: String,
    /// Toast level
    pub level: ToastLevel,
    /// When the toast was created
    pub created_at: Instant,
    /// How long the toast should be displayed
    pub duration: Duration,
}

impl Toast {
    /// Create a new toast
    pub fn new(message: impl Into<String>, level: ToastLevel) -> Self {
        Self {
            message: message.into(),
            level,
            created_at: Instant::now(),
            duration: Duration::from_secs(3),
        }
    }

    /// Create a toast with custom duration
    pub fn with_duration(
        message: impl Into<String>,
        level: ToastLevel,
        duration: Duration,
    ) -> Self {
        Self {
            message: message.into(),
            level,
            created_at: Instant::now(),
            duration,
        }
    }

    /// Create an info toast
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(message, ToastLevel::Info)
    }

    /// Create a success toast
    pub fn success(message: impl Into<String>) -> Self {
        Self::new(message, ToastLevel::Success)
    }

    /// Create a warning toast
    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(message, ToastLevel::Warning)
    }

    /// Create an error toast
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(message, ToastLevel::Error)
    }

    /// Check if the toast has expired
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.duration
    }

    /// Get remaining time as a fraction (1.0 = just created, 0.0 = expired)
    pub fn remaining_fraction(&self) -> f32 {
        let elapsed = self.created_at.elapsed();
        if elapsed >= self.duration {
            0.0
        } else {
            1.0 - (elapsed.as_secs_f32() / self.duration.as_secs_f32())
        }
    }

    /// Render the toast as a Line
    pub fn render(&self) -> Line<'static> {
        let color = self.level.color();
        let icon = self.level.icon();

        // Calculate fade effect based on remaining time
        let _fraction = self.remaining_fraction();

        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{} ", icon), Style::default()),
            Span::styled(
                self.message.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])
    }
}

/// Toast manager for handling multiple toasts
#[derive(Clone, Debug)]
pub struct ToastManager {
    /// Active toasts
    pub toasts: Vec<Toast>,
    /// Maximum number of visible toasts
    pub max_visible: usize,
}

impl ToastManager {
    /// Create a new toast manager
    pub fn new() -> Self {
        Self {
            toasts: Vec::new(),
            max_visible: 3,
        }
    }

    /// Add a toast
    pub fn add(&mut self, toast: Toast) {
        self.toasts.push(toast);
        // Remove expired toasts
        self.cleanup();
        // Limit visible toasts
        while self.toasts.len() > self.max_visible {
            self.toasts.remove(0);
        }
    }

    /// Remove expired toasts
    pub fn cleanup(&mut self) {
        self.toasts.retain(|t| !t.is_expired());
    }

    /// Check if there are any active toasts
    pub fn has_toasts(&self) -> bool {
        !self.toasts.is_empty()
    }

    /// Render all active toasts
    pub fn render(&self) -> Vec<Line<'static>> {
        self.toasts.iter().map(|t| t.render()).collect()
    }

    /// Clear all toasts
    pub fn clear(&mut self) {
        self.toasts.clear();
    }
}

impl Default for ToastManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_toast_creation() {
        let toast = Toast::info("Test message");
        assert_eq!(toast.message, "Test message");
        assert_eq!(toast.level, ToastLevel::Info);
        assert!(!toast.is_expired());
    }

    #[test]
    fn test_toast_levels() {
        let info = Toast::info("info");
        let success = Toast::success("success");
        let warning = Toast::warning("warning");
        let error = Toast::error("error");

        assert_eq!(info.level, ToastLevel::Info);
        assert_eq!(success.level, ToastLevel::Success);
        assert_eq!(warning.level, ToastLevel::Warning);
        assert_eq!(error.level, ToastLevel::Error);
    }

    #[test]
    fn test_toast_expiration() {
        let toast = Toast::with_duration("test", ToastLevel::Info, Duration::from_millis(50));
        assert!(!toast.is_expired());

        sleep(Duration::from_millis(100));
        assert!(toast.is_expired());
    }

    #[test]
    fn test_toast_manager() {
        let mut manager = ToastManager::new();
        assert!(!manager.has_toasts());

        manager.add(Toast::info("test 1"));
        manager.add(Toast::success("test 2"));
        assert!(manager.has_toasts());
        assert_eq!(manager.toasts.len(), 2);

        manager.clear();
        assert!(!manager.has_toasts());
    }

    #[test]
    fn test_toast_manager_max_visible() {
        let mut manager = ToastManager::new();
        manager.max_visible = 2;

        manager.add(Toast::info("test 1"));
        manager.add(Toast::success("test 2"));
        manager.add(Toast::warning("test 3"));

        // Should only keep the last 2
        assert_eq!(manager.toasts.len(), 2);
    }

    #[test]
    fn test_toast_render() {
        let toast = Toast::info("Test message");
        let line = toast.render();

        // Should have spans
        assert!(!line.spans.is_empty());

        // Check that message is included
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("Test message"));
    }

    #[test]
    fn test_toast_manager_cleanup() {
        let mut manager = ToastManager::new();

        // Add a toast with short duration
        manager.add(Toast::with_duration(
            "short",
            ToastLevel::Info,
            Duration::from_millis(50),
        ));

        // Add a toast with longer duration
        manager.add(Toast::with_duration(
            "long",
            ToastLevel::Success,
            Duration::from_secs(10),
        ));

        assert_eq!(manager.toasts.len(), 2);

        // Wait for short toast to expire
        sleep(Duration::from_millis(100));

        // Cleanup should remove expired toast
        manager.cleanup();
        assert_eq!(manager.toasts.len(), 1);
        assert_eq!(manager.toasts[0].message, "long");
    }
}
