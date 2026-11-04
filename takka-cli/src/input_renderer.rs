//! Input renderer — renders prompt, input, and dropdown to stdout via inline.rs.
//!
//! Uses transient rendering (overwrite in-place) so the prompt stays at the
//! bottom of the terminal while the user types.

use crate::input_buffer::InputBuffer;
use crate::tui::dropdown::{Dropdown, DropdownType};
use crate::tui::input::{render_input, render_placeholder};
use crate::widgets::inline;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Number of physical terminal rows a `Line` occupies after the terminal
/// wraps it at the current column width.
///
/// Styled ANSI escape sequences are zero-width and [`Line::width`] uses
/// unicode display widths, so this matches what the terminal renders even
/// for emoji / CJK. A `Line` of exactly `cols` characters does **not** wrap
/// (the terminal fills the last column, then `\r\n` moves cleanly to the
/// next row), hence `div_ceil`.
fn physical_lines(line: &Line<'_>) -> usize {
    wrapped_line_count(line.width(), terminal_cols())
}

/// Pure wrap math: how many rows a string of display `width` occupies in a
/// terminal `cols` columns wide. Always at least 1 (the row itself).
fn wrapped_line_count(width: usize, cols: usize) -> usize {
    width.div_ceil(cols.max(1)).max(1)
}

/// Actual terminal width in columns. Falls back to 80 when it cannot be
/// determined. Unlike `inline::terminal_width()` there is no 40-column
/// floor: underestimating the width would undercount wrapped rows and
/// break the REPL's MoveUp(N) clearing on narrow terminals.
fn terminal_cols() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| (w as usize).max(1))
        .unwrap_or(80)
}

/// Metadata shown in the prompt area
pub struct PromptMetadata {
    pub dir_name: String,
    pub provider: String,
    pub model: String,
    pub git_branch: Option<String>,
}

impl PromptMetadata {
    pub fn new(provider: String, model: String) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        let dir_name = cwd
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();

        let git_branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if b.is_empty() || b == "HEAD" {
                        None
                    } else {
                        Some(b)
                    }
                } else {
                    None
                }
            });

        Self {
            dir_name,
            provider,
            model,
            git_branch,
        }
    }
}

fn model_status_line(meta: &PromptMetadata) -> Line<'static> {
    Line::from(vec![
        Span::styled("Model: ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(
            format!("{}/{}", meta.provider, meta.model),
            Style::default().fg(Color::Cyan),
        ),
    ])
}

/// Render the model status line followed by the prompt + input to stdout.
/// Overwrites the previous prompt render in-place.
///
/// Returns the number of *physical* terminal lines rendered (including
/// continuation lines and footer for multi-line mode). Long input wraps
/// onto extra rows, so the count is the sum of wrapped rows, not the
/// number of logical `print_line` calls — the REPL loop needs this exact
/// count for its MoveUp(N) clearing. The cursor is left on a NEW line
/// below the prompt so the REPL loop can MoveUp(N) correctly.
pub fn render_prompt_line(
    meta: &PromptMetadata,
    buffer: &InputBuffer,
    _has_dropdown: bool,
) -> usize {
    let dim = Style::default().add_modifier(Modifier::DIM);

    let model_line = model_status_line(meta);
    inline::print_line(&model_line);
    let model_physical = physical_lines(&model_line);

    // Build prompt prefix: "dirname> "
    let prompt_prefix = format!("{}> ", meta.dir_name);

    // Check if multi-line mode
    if buffer.is_multiline() {
        // Render multi-line input
        let lines = buffer.lines();
        let current_line = buffer.current_line();
        let mut physical = model_physical;

        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                // First line with prompt
                let input_line = render_input(line, buffer.cursor().min(line.len()));
                let mut spans: Vec<Span<'static>> = vec![Span::styled(prompt_prefix.clone(), dim)];
                spans.extend(input_line.spans);
                let rendered = Line::from(spans);
                inline::print_line(&rendered);
                physical += physical_lines(&rendered);
            } else {
                // Continuation lines with indent
                let indent = "  "; // 2 spaces for continuation
                let line_cursor = if i == current_line {
                    // Calculate cursor position within this line
                    let lines_before: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
                    if buffer.cursor() > lines_before {
                        buffer.cursor() - lines_before
                    } else {
                        0
                    }
                } else {
                    0
                };

                let input_line = render_input(line, line_cursor.min(line.len()));
                let mut spans: Vec<Span<'static>> = vec![
                    Span::styled(indent.to_string(), dim),
                    Span::styled(
                        "│ ".to_string(),
                        Style::default().fg(Color::Rgb(100, 100, 120)),
                    ),
                ];
                spans.extend(input_line.spans);
                let rendered = Line::from(spans);
                inline::print_line(&rendered);
                physical += physical_lines(&rendered);
            }
        }

        // Show multi-line indicator
        let indicator = Line::from(vec![
            Span::styled("  ", dim),
            Span::styled(
                format!(
                    "[{} lines] Shift+Enter: new line, Enter: submit",
                    lines.len()
                ),
                Style::default()
                    .fg(Color::Rgb(100, 100, 120))
                    .add_modifier(Modifier::DIM),
            ),
        ]);
        inline::print_line(&indicator);
        physical += physical_lines(&indicator);

        physical
    } else {
        // Single-line mode
        // NOTE: We use `print_line` (which appends `\r\n`) instead of
        // `print_transient` so that the cursor ends up on the NEXT line
        // after the prompt.  This lets the REPL loop's `MoveUp(N)`
        // correctly return to the prompt line rather than overshooting
        // to the status bar above it.
        let input_line = if buffer.is_empty() {
            render_placeholder()
        } else {
            render_input(buffer.text(), buffer.cursor())
        };

        // Combine: prompt prefix + input content
        let mut spans: Vec<Span<'static>> = vec![Span::styled(prompt_prefix, dim)];
        spans.extend(input_line.spans);

        let rendered = Line::from(spans);
        inline::print_line(&rendered);
        model_physical + physical_lines(&rendered)
    }
}

/// Render dropdown items below the prompt line.
/// Returns the number of lines printed (for clearing later).
pub fn render_dropdown_lines(dropdown: &Dropdown) -> usize {
    if dropdown.is_empty() {
        return 0;
    }

    let visible = dropdown.visible_items();
    let query = dropdown.query();
    let icon = match dropdown.dropdown_type {
        DropdownType::Command => "⌘",
        DropdownType::File => "📁",
        DropdownType::Model => "🤖",
        DropdownType::Skill => "⚡",
    };

    // Title line
    let title_style = Style::default()
        .fg(Color::Rgb(241, 196, 15))
        .add_modifier(Modifier::BOLD);
    let title = Line::from(vec![Span::styled(
        format!("  {} {} ", icon, dropdown.title()),
        title_style,
    )]);
    inline::print_line(&title);

    // Count *physical* rows: a long item (e.g. a model display name with
    // description) wraps, and the REPL loop must clear back to the top of
    // the dropdown on the next keystroke.
    let mut count = physical_lines(&title);

    for (i, item, selected) in &visible {
        let base_style = if *selected {
            Style::default()
                .bg(Color::Rgb(52, 152, 219))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(200, 200, 200))
        };

        let item_icon = match dropdown.dropdown_type {
            DropdownType::File => {
                if item.ends_with('/') {
                    "📁 "
                } else {
                    "📄 "
                }
            }
            _ => "  ",
        };

        // Build item text with fuzzy highlighting
        let mut spans = vec![Span::styled(format!("  {}", item_icon), base_style)];

        if !query.is_empty() {
            // Apply fuzzy match highlighting
            spans.extend(fuzzy_match_highlight(item, query, *selected));
        } else {
            spans.push(Span::styled(item.to_string(), base_style));
        }

        if let Some(desc) = dropdown.get_description(*i) {
            let desc_style = if *selected {
                Style::default()
                    .bg(Color::Rgb(52, 152, 219))
                    .fg(Color::Rgb(200, 200, 200))
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(format!("  {}", desc), desc_style));
        }

        let rendered = Line::from(spans);
        inline::print_line(&rendered);
        count += physical_lines(&rendered);
    }

    count
}

/// Highlight fuzzy match characters in text.
///
/// Returns a vector of spans with matched characters highlighted.
/// Case-insensitive matching is used.
fn fuzzy_match_highlight(text: &str, query: &str, is_selected: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let query_chars: Vec<char> = query.to_lowercase().chars().collect();
    let mut qi = 0;

    let normal_style = if is_selected {
        Style::default()
            .bg(Color::Rgb(52, 152, 219))
            .fg(Color::White)
    } else {
        Style::default().fg(Color::Rgb(200, 200, 200))
    };

    let highlight_style = if is_selected {
        Style::default()
            .bg(Color::Rgb(52, 152, 219))
            .fg(Color::Rgb(255, 215, 0))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Rgb(255, 215, 0))
            .add_modifier(Modifier::BOLD)
    };

    for c in text.chars() {
        if qi < query_chars.len() && c.to_lowercase().next() == Some(query_chars[qi]) {
            // Highlight matched character
            spans.push(Span::styled(c.to_string(), highlight_style));
            qi += 1;
        } else {
            // Normal character
            spans.push(Span::styled(c.to_string(), normal_style));
        }
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_status_line_includes_provider_and_model() {
        let meta = PromptMetadata {
            dir_name: "project".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            git_branch: None,
        };

        assert_eq!(model_status_line(&meta).to_string(), "Model: openai/gpt-4o");
    }

    #[test]
    fn test_wrapped_line_count_single_row() {
        assert_eq!(wrapped_line_count(0, 80), 1); // empty line still occupies a row
        assert_eq!(wrapped_line_count(1, 80), 1);
        assert_eq!(wrapped_line_count(79, 80), 1);
        // Exactly one full row does NOT wrap (last column filled, \r\n
        // moves cleanly to the next row).
        assert_eq!(wrapped_line_count(80, 80), 1);
    }

    #[test]
    fn test_wrapped_line_count_multi_row() {
        assert_eq!(wrapped_line_count(81, 80), 2);
        assert_eq!(wrapped_line_count(160, 80), 2);
        assert_eq!(wrapped_line_count(161, 80), 3);
    }

    #[test]
    fn test_wrapped_line_count_narrow_terminal() {
        assert_eq!(wrapped_line_count(30, 30), 1);
        assert_eq!(wrapped_line_count(31, 30), 2);
    }

    #[test]
    fn test_wrapped_line_count_unicode_width() {
        // CJK characters have display width 2 in unicode-width.
        let line = Line::from(Span::raw("你好"));
        assert_eq!(line.width(), 4);
        assert_eq!(wrapped_line_count(line.width(), 80), 1);
        // Wide chars near the boundary count toward wrapping.
        assert_eq!(wrapped_line_count(line.width(), 3), 2);
    }

    #[test]
    fn test_physical_lines_styled_ignores_ansi() {
        // Styling must not affect the row count. Verify this against the
        // `Line`'s own width (which must exclude the zero-width ANSI escapes)
        // and a fixed column count, since `physical_lines` reads the ambient
        // terminal width (which is not 80 on headless CI runners).
        let styled = Line::from(Span::styled(
            "x".repeat(81),
            Style::default().fg(Color::Rgb(1, 2, 3)),
        ));
        assert_eq!(styled.width(), 81);
        assert_eq!(wrapped_line_count(styled.width(), 80), 2);
    }
}
