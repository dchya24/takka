//! Tool call / tool result rendering.
//!
//! Produces `Vec<Line<'static>>` from a tool name + arguments + optional
//! result. Used by both the TUI message log and any inline trace mode.
//!
//! Visual contract:
//!
//! ```text
//!   ╭─ tool · read_file ──────────────────────────╮
//!   │  path = "src/main.rs"                       │
//!   │  limit = 100                                │
//!   ╰─────────────────────────────────────────────╯
//!   ┃ ✓ output  (3 lines, 142 bytes)
//!     line 1...
//! ```

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use super::components::{notification, panel, BoxStyle};

// ── Compact inline rendering (interactive mode) ────────────
//
// Single-line tool call + result rendering for scrollback efficiency.
// Used by inline/interactive mode. The full panel rendering (TUI mode)
// remains in render_call() / render_result() below.

/// Maximum total width for the compact tool call line.
const COMPACT_MAX_WIDTH: usize = 80;

/// Render a compact single-line tool call.
///
/// Format: ` ⚙ tool_name(path="src/main.rs", limit=100)`
/// Truncates args if the total line exceeds COMPACT_MAX_WIDTH.
pub fn render_call_compact(tool_name: &str, arguments: &Value) -> Line<'static> {
    let icon_style = Style::default()
        .fg(Color::Rgb(241, 196, 15))
        .add_modifier(Modifier::BOLD);
    let name_style = Style::default()
        .fg(Color::Rgb(52, 152, 219))
        .add_modifier(Modifier::BOLD);
    let dim_style = Style::default()
        .fg(Color::Rgb(180, 180, 200))
        .add_modifier(Modifier::DIM);

    let args_str = compact_args(arguments);

    // Truncate args if total line would be too wide
    let truncated_args = if tool_name.len() + args_str.len() + 6 > COMPACT_MAX_WIDTH {
        let available = COMPACT_MAX_WIDTH
            .saturating_sub(tool_name.len() + 6) // " ⚙ name(" ... ")"
            .saturating_sub(1); // "…" char
        if available > 0 && available < args_str.len() {
            // Find a safe truncation point (don't split multi-byte)
            let end = args_str
                .char_indices()
                .take_while(|(i, _)| *i < available)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            format!("{}…", &args_str[..end])
        } else {
            args_str
        }
    } else {
        args_str
    };

    Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{2699}", icon_style),
        Span::raw(" "),
        Span::styled(tool_name.to_string(), name_style),
        Span::styled("(", dim_style),
        Span::styled(truncated_args, dim_style),
        Span::styled(")", dim_style),
    ])
}

/// Render a compact result line.
///
/// Format: `   → ✓ 142 lines` or `   → ✗ error: permission denied`
pub fn render_result_compact(output: &Value, is_error: bool) -> Line<'static> {
    if is_error {
        let body = match output {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let msg = if body.len() > 60 {
            format!("{}\u{2026}", &body[..57])
        } else {
            body
        };
        return Line::from(vec![
            Span::raw("   "),
            Span::styled(
                "\u{2192}",
                Style::default()
                    .fg(Color::Rgb(180, 180, 200))
                    .add_modifier(Modifier::DIM),
            ),
            Span::raw(" "),
            Span::styled(
                "\u{2717}",
                Style::default()
                    .fg(Color::Rgb(231, 76, 60))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(msg, Style::default().fg(Color::Rgb(231, 76, 60))),
        ]);
    }

    let summary = result_summary(output);
    Line::from(vec![
        Span::raw("   "),
        Span::styled(
            "\u{2192}",
            Style::default()
                .fg(Color::Rgb(180, 180, 200))
                .add_modifier(Modifier::DIM),
        ),
        Span::raw(" "),
        Span::styled(
            "\u{2713}",
            Style::default()
                .fg(Color::Rgb(46, 204, 113))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(summary, Style::default().fg(Color::Rgb(46, 204, 113))),
    ])
}

/// Format arguments as a compact inline string.
/// e.g. `path="src/main.rs", limit=100`
fn compact_args(arguments: &Value) -> String {
    match arguments {
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}={}", k, value_inline(v)))
                .collect();
            parts.join(", ")
        }
        Value::Null => String::new(),
        other => value_inline(other),
    }
}

// ── Full panel rendering (TUI mode) ────────────────────────

/// Render a tool call: a bordered panel with the tool name as title and
/// the arguments listed as `key = value` rows.
pub fn render_call(tool_name: &str, arguments: &Value) -> Vec<Line<'static>> {
    let title = format!("tool · {}", tool_name);
    let body = arg_lines(arguments);
    panel(&title, &body, BoxStyle::Rounded, Color::Rgb(241, 196, 15))
}

/// Render the result of a tool call.
///
/// `verbose` controls whether the output body is shown:
/// - `false` (default for inline mode): only the notification headline,
///   keeping the scrollback compact when many tools run in sequence.
///   Errors are always shown with their message.
/// - `true`: the full output body, truncated at `max_body_lines`.
pub fn render_result(
    tool_name: &str,
    output: &Value,
    is_error: bool,
    max_body_lines: usize,
    verbose: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let (icon, color, summary) = if is_error {
        (
            "✗",
            Color::Rgb(231, 76, 60),
            format!("error from {}", tool_name),
        )
    } else {
        let summary = result_summary(output);
        (
            "✓",
            Color::Rgb(46, 204, 113),
            format!("{}  {}", tool_name, summary),
        )
    };
    lines.push(notification(icon, &summary, color));

    // Errors always show their message regardless of `verbose`. Successful
    // calls only render the body when verbose mode is on.
    if !is_error && !verbose {
        return lines;
    }

    let body_text = match output {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };

    let dim = Style::default().add_modifier(Modifier::DIM);
    let total_lines = body_text.lines().count();
    for (shown, raw_line) in body_text.lines().enumerate() {
        if shown >= max_body_lines {
            let remaining = total_lines.saturating_sub(shown);
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("… {} more line(s) truncated", remaining), dim),
            ]));
            break;
        }
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::raw(raw_line.to_string()),
        ]));
    }

    lines
}

// ── Helpers ─────────────────────────────────────────────────

fn arg_lines(arguments: &Value) -> Vec<Line<'static>> {
    let key_style = Style::default()
        .fg(Color::Rgb(241, 196, 15))
        .add_modifier(Modifier::BOLD);
    let eq_style = Style::default().add_modifier(Modifier::DIM);

    match arguments {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                Line::from(vec![
                    Span::styled(k.clone(), key_style),
                    Span::styled(" = ", eq_style),
                    Span::raw(value_inline(v)),
                ])
            })
            .collect(),
        Value::Null => vec![Line::from(Span::styled(
            "(no arguments)",
            Style::default().add_modifier(Modifier::DIM),
        ))],
        other => vec![Line::from(Span::raw(value_inline(other)))],
    }
}

/// Compact one-line representation of a JSON value, suitable for arg rows.
fn value_inline(v: &Value) -> String {
    match v {
        Value::String(s) => format!("\"{}\"", s),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => {
            // Show the first few elements only.
            let preview: Vec<String> = a.iter().take(3).map(value_inline).collect();
            let more = if a.len() > 3 {
                format!(", … +{}", a.len() - 3)
            } else {
                String::new()
            };
            format!("[{}{}]", preview.join(", "), more)
        }
        Value::Object(_) => {
            let s = v.to_string();
            if s.len() > 80 {
                format!("{}…", &s[..79])
            } else {
                s
            }
        }
    }
}

/// One-line summary of a tool result, used in the success notification.
fn result_summary(output: &Value) -> String {
    match output {
        Value::String(s) => {
            let lines = s.lines().count();
            format!(
                "({} line{}, {} byte{})",
                lines,
                if lines == 1 { "" } else { "s" },
                s.len(),
                if s.len() == 1 { "" } else { "s" },
            )
        }
        Value::Array(a) => format!("({} item{})", a.len(), if a.len() == 1 { "" } else { "s" }),
        Value::Object(o) => format!("({} field{})", o.len(), if o.len() == 1 { "" } else { "s" }),
        Value::Null => "(no output)".into(),
        _ => format!("({})", output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn flatten(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn render_call_includes_tool_name_and_args() {
        let lines = render_call("read_file", &json!({"path": "src/main.rs", "limit": 100}));
        let body: String = lines.iter().map(flatten).collect::<Vec<_>>().join("\n");
        assert!(body.contains("read_file"));
        assert!(body.contains("path"));
        assert!(body.contains("\"src/main.rs\""));
        assert!(body.contains("limit"));
        assert!(body.contains("100"));
    }

    #[test]
    fn render_call_with_no_args() {
        let lines = render_call("status", &Value::Null);
        let body: String = lines.iter().map(flatten).collect::<Vec<_>>().join("\n");
        assert!(body.contains("status"));
        assert!(body.contains("no arguments"));
    }

    #[test]
    fn render_result_truncates_long_output() {
        let big = (0..50)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_result(
            "bash",
            &Value::String(big),
            false,
            10,
            /*verbose=*/ true,
        );
        let truncation_marker = lines
            .iter()
            .map(flatten)
            .any(|l| l.contains("more line(s) truncated"));
        assert!(truncation_marker);
    }

    #[test]
    fn render_result_compact_omits_body_for_success() {
        let big = (0..50)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_result(
            "bash",
            &Value::String(big),
            false,
            10,
            /*verbose=*/ false,
        );
        // Only the headline notification line, no body.
        assert_eq!(lines.len(), 1);
        assert!(flatten(&lines[0]).contains("✓"));
    }

    #[test]
    fn render_result_error_uses_red_accent() {
        let lines = render_result(
            "bash",
            &Value::String("permission denied".into()),
            true,
            5,
            false,
        );
        // The first line is the notification accent — should contain ✗.
        let first = flatten(&lines[0]);
        assert!(first.contains("✗"));
        assert!(first.contains("error from bash"));
    }

    #[test]
    fn render_result_error_always_shows_body() {
        // Errors should show their message even when verbose=false so
        // the user can debug without needing a flag.
        let lines = render_result(
            "bash",
            &Value::String("permission denied".into()),
            true,
            5,
            false,
        );
        assert!(lines
            .iter()
            .map(flatten)
            .any(|l| l.contains("permission denied")));
    }

    #[test]
    fn render_call_compact_single_line() {
        let line = render_call_compact("read_file", &json!({"path": "src/main.rs", "limit": 100}));
        let text = flatten(&line);
        assert!(text.contains("\u{2699}"), "should contain gear icon");
        assert!(text.contains("read_file"));
        assert!(text.contains("path"));
        assert!(text.contains("src/main.rs"));
    }

    #[test]
    fn render_call_compact_no_args() {
        let line = render_call_compact("status", &Value::Null);
        let text = flatten(&line);
        assert!(text.contains("status"));
        assert!(text.contains("("));
        assert!(text.contains(")"));
    }

    #[test]
    fn render_call_compact_truncates_long_args() {
        let long_path = "x".repeat(200);
        let line = render_call_compact("read_file", &json!({"path": long_path}));
        let text = flatten(&line);
        // Should be truncated — not the full 200-char string
        assert!(text.len() < 200);
        assert!(
            text.contains("\u{2026}") || text.len() < 100,
            "should truncate with ellipsis"
        );
    }

    #[test]
    fn render_result_compact_success() {
        let line = render_result_compact(&Value::String("hello\nworld".into()), false);
        let text = flatten(&line);
        assert!(text.contains("\u{2713}"), "should contain checkmark");
        assert!(text.contains("2 lines"));
    }

    #[test]
    fn render_result_compact_error() {
        let line =
            render_result_compact(&Value::String("Tool error: permission denied".into()), true);
        let text = flatten(&line);
        assert!(text.contains("\u{2717}"), "should contain X mark");
        assert!(text.contains("permission denied"));
    }

    #[test]
    fn value_inline_truncates_arrays() {
        let s = value_inline(&json!([1, 2, 3, 4, 5]));
        assert!(s.contains("+2"));
    }
}
