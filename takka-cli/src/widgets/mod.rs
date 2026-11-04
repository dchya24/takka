//! Shared widgets for both TUI (full-screen) and CLI (inline) rendering.
//!
//! Components in this module produce ratatui primitives (`Line`, `Span`, `Text`)
//! that can be consumed by:
//! - The full-screen TUI via `ratatui::Terminal`
//! - The CLI via `inline::print_lines()` which writes styled output directly to stdout

pub mod capabilities;
pub mod code_highlight;
pub mod components;
pub mod diff;
pub mod inline;
pub mod markdown;
pub mod progress;
pub mod spinner;
pub mod toast;
pub mod tool_call;
