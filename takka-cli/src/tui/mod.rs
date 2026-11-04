//! TUI module for interactive mode using ratatui
//!
//! Features:
//! - Padded UI layout
//! - Rich markdown rendering (via shared widgets)
//! - Animated progress indicators (via shared widgets)
//! - Dropdown for `/` commands
//! - Dropdown for `@` file completion

mod app;
pub mod dropdown;
pub mod input;
mod ui;

pub use app::run_tui;
