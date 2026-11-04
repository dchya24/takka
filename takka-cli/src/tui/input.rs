//! Input handling and rendering utilities
//!
//! Provides:
//! - Syntax-highlighted input rendering with cursor
//! - `@` reference highlighting (blue)
//! - `/` command highlighting (yellow)
//! - Placeholder text when input is empty

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Render input with syntax highlighting and cursor
pub fn render_input(input: &str, cursor_pos: usize) -> Line<'static> {
    if input.is_empty() {
        return Line::from(vec![Span::styled(" ", Style::default().bg(Color::White))]);
    }

    let mut spans = Vec::new();
    let chars: Vec<(usize, char)> = input.char_indices().collect();

    // Build style regions
    for (byte_pos, c) in chars.iter() {
        let style = get_char_style(*c, input, *byte_pos);
        let is_cursor = *byte_pos == cursor_pos;

        let char_style = if is_cursor {
            Style::default().bg(Color::White).fg(Color::Black)
        } else {
            style
        };

        spans.push(Span::styled(c.to_string(), char_style));
    }

    // If cursor is at the very end (after last char), add a cursor block
    if cursor_pos >= input.len() {
        spans.push(Span::styled(" ", Style::default().bg(Color::White)));
    }

    Line::from(spans)
}

/// Get style for a character based on context
fn get_char_style(c: char, input: &str, pos: usize) -> Style {
    // ── Slash command highlighting ──
    if input.starts_with('/') {
        if pos == 0 {
            return Style::default()
                .fg(Color::Rgb(241, 196, 15))
                .add_modifier(Modifier::BOLD);
        }
        // Highlight the command name (until first space)
        if let Some(space_pos) = input.find(' ') {
            if pos < space_pos {
                return Style::default().fg(Color::Rgb(241, 196, 15));
            }
        } else {
            // No space yet — whole thing is command
            return Style::default().fg(Color::Rgb(241, 196, 15));
        }
    }

    // ── @ file reference highlighting ──
    if c == '@' {
        return Style::default()
            .fg(Color::Rgb(52, 152, 219))
            .add_modifier(Modifier::BOLD);
    }

    // Check if we're inside an @reference
    let before = &input[..pos];
    if let Some(at_pos) = before.rfind('@') {
        // @ must be at start or after whitespace
        let at_valid = at_pos == 0 || input[..at_pos].ends_with(char::is_whitespace);
        if at_valid {
            // Check no space between @ and current position
            if !input[at_pos..pos + c.len_utf8()].contains(char::is_whitespace) {
                return Style::default().fg(Color::Rgb(52, 152, 219));
            }
        }
    }

    Style::default().fg(Color::White)
}

/// Render placeholder text when input is empty
pub fn render_placeholder() -> Line<'static> {
    Line::from(vec![
        Span::styled("Type a message, ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "/",
            Style::default()
                .fg(Color::Rgb(241, 196, 15))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" for commands, ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "@",
            Style::default()
                .fg(Color::Rgb(52, 152, 219))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" for files", Style::default().fg(Color::DarkGray)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_empty_input() {
        let line = render_input("", 0);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn test_render_slash_command() {
        let line = render_input("/help", 5);
        assert!(!line.spans.is_empty());
        // Cursor should be at end (cursor_pos == input.len())
        let last = line.spans.last().unwrap();
        assert_eq!(last.content, " "); // cursor block
    }

    #[test]
    fn test_render_at_reference() {
        let line = render_input("read @src/main.rs", 17);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn test_render_cursor_in_middle() {
        let line = render_input("hello", 2);
        // Should have 5 chars + maybe end cursor
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn test_render_slash_with_arg() {
        let line = render_input("/help me", 8);
        assert!(!line.spans.is_empty());
    }
}
