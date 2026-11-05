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

// ── Inline timeline rendering (interactive mode) ───────────
//
// One row per finished tool call, laid out as a timeline so a long run reads
// as a sequence of steps instead of three loose lines per tool:
//
//   ├ ✓ read_file  src/main.rs              12 lines · 120ms
//   ├ ✗ run_command  cargo build                    1.2s
//   │   error[E0425]: cannot find value `x` in this scope
//   ├ ✓ edit_file  src/main.rs              +4 −2 · 60ms
//
// Nested detail (streamed output, errors, diffs) hangs off the `│` guide.

/// Branch prefix for a finished tool row.
pub const TIMELINE_BRANCH: &str = "  \u{251c} ";
/// Guide prefix for lines that belong to the row above.
pub const TIMELINE_DETAIL: &str = "  \u{2502}   ";

/// Widest the timeline rows are allowed to get (keeps the duration column
/// from drifting to the far right of a very wide terminal).
const TIMELINE_MAX_WIDTH: usize = 96;

/// Arguments worth showing before the tool name: the first one present wins.
/// Ordered by how much it tells the reader about *what the tool touched*.
const HEADLINE_ARGS: &[&str] = &[
    "path",
    "file_path",
    "command",
    "pattern",
    "query",
    "url",
    "name",
    "task",
    "goal",
    "prompt",
];

/// Render one finished tool call as a timeline row.
///
/// `summary` is an optional short result note (`12 lines`, `+4 \u{2212}2`);
/// `duration_ms` is right-aligned as the timing column.
pub fn render_timeline_row(
    tool_name: &str,
    arguments: Option<&Value>,
    success: bool,
    summary: Option<&str>,
    diff_counts: Option<(u32, u32)>,
    duration_ms: u64,
    truncated: bool,
) -> Line<'static> {
    let icon_style = if success {
        Style::default()
            .fg(Color::Rgb(46, 204, 113))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Rgb(231, 76, 60))
            .add_modifier(Modifier::BOLD)
    };
    let name_style = Style::default()
        .fg(Color::Rgb(52, 152, 219))
        .add_modifier(Modifier::BOLD);
    let dim_style = Style::default()
        .fg(Color::Rgb(160, 160, 180))
        .add_modifier(Modifier::DIM);

    let target_width = super::inline::terminal_width()
        .saturating_sub(2)
        .clamp(40, TIMELINE_MAX_WIDTH);

    let icon = if success { "\u{2713}" } else { "\u{2717}" };
    let branch = format!("{}{} ", TIMELINE_BRANCH, icon);
    let headline = arguments.map(arg_headline).unwrap_or_default();

    let left_width = branch.chars().count()
        + tool_name.chars().count()
        + if headline.is_empty() {
            0
        } else {
            headline.chars().count() + 2
        };

    // Right column: result note, diff counts, truncation marker, duration.
    let mut right_bits: Vec<String> = Vec::new();
    if let Some(summary) = summary {
        if !summary.is_empty() {
            right_bits.push(summary.to_string());
        }
    }
    if let Some((added, removed)) = diff_counts {
        if added + removed > 0 {
            right_bits.push(format!("+{} \u{2212}{}", added, removed));
        }
    }
    if truncated {
        right_bits.push("truncated".to_string());
    }
    // `0ms` means the runtime didn't measure the call — better to omit the
    // column than to claim the tool was instant.
    if duration_ms > 0 {
        right_bits.push(format_duration(duration_ms));
    }
    let right = right_bits.join(" \u{b7} ");

    let pad = target_width
        .saturating_sub(left_width + right.chars().count())
        .max(1);

    let mut spans = vec![
        Span::styled(branch, icon_style),
        Span::styled(tool_name.to_string(), name_style),
    ];
    if !headline.is_empty() {
        spans.push(Span::styled(format!("  {}", headline), dim_style));
    }
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, dim_style));

    Line::from(spans)
}

/// Row for a tool that is still running and streaming output.
///
/// Printed on the first `ToolDelta` so the streamed lines have a parent row to
/// hang from; the outcome arrives later as a nested detail line (see
/// [`render_outcome_detail`]).
pub fn render_running_row(tool_name: &str, arguments: Option<&Value>) -> Line<'static> {
    let icon_style = Style::default()
        .fg(Color::Rgb(241, 196, 15))
        .add_modifier(Modifier::BOLD);
    let name_style = Style::default()
        .fg(Color::Rgb(52, 152, 219))
        .add_modifier(Modifier::BOLD);
    let dim_style = Style::default()
        .fg(Color::Rgb(160, 160, 180))
        .add_modifier(Modifier::DIM);

    let mut spans = vec![
        Span::styled(format!("{}\u{27f3} ", TIMELINE_BRANCH), icon_style),
        Span::styled(tool_name.to_string(), name_style),
    ];
    if let Some(args) = arguments {
        let headline = arg_headline(args);
        if !headline.is_empty() {
            spans.push(Span::styled(format!("  {}", headline), dim_style));
        }
    }
    spans.push(Span::styled("  running\u{2026}", dim_style));
    Line::from(spans)
}

/// Nested outcome line for a streamed tool: `✓ 6 lines \u{b7} 2ms`.
pub fn render_outcome_detail(
    success: bool,
    summary: Option<&str>,
    duration_ms: u64,
) -> Line<'static> {
    let (icon, color) = if success {
        ("\u{2713}", Color::Rgb(46, 204, 113))
    } else {
        ("\u{2717}", Color::Rgb(231, 76, 60))
    };
    let mut bits: Vec<String> = Vec::new();
    if let Some(summary) = summary {
        if !summary.is_empty() {
            bits.push(summary.to_string());
        }
    }
    if duration_ms > 0 {
        bits.push(format_duration(duration_ms));
    }
    let detail = if bits.is_empty() {
        icon.to_string()
    } else {
        format!("{} {}", icon, bits.join(" \u{b7} "))
    };
    timeline_detail(&Line::from(Span::styled(
        detail,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )))
}

/// Wrap a nested detail line in the timeline guide, keeping its content.
pub fn timeline_detail(line: &Line<'_>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        TIMELINE_DETAIL.to_string(),
        Style::default()
            .fg(Color::Rgb(90, 90, 110))
            .add_modifier(Modifier::DIM),
    )];
    spans.extend(
        line.spans
            .iter()
            .map(|span| Span::styled(span.content.to_string(), span.style)),
    );
    Line::from(spans)
}

/// Detail lines for an error result: message body, capped at `max_lines`.
pub fn error_detail_lines(message: &str, max_lines: usize) -> Vec<Line<'static>> {
    let style = Style::default()
        .fg(Color::Rgb(231, 76, 60))
        .add_modifier(Modifier::DIM);
    let mut lines: Vec<Line<'static>> = message
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(max_lines)
        .map(|l| timeline_detail(&Line::from(Span::styled(l.to_string(), style))))
        .collect();
    let total = message.lines().filter(|l| !l.trim().is_empty()).count();
    if total > max_lines {
        lines.push(timeline_detail(&Line::from(Span::styled(
            format!("\u{2026} {} more line(s)", total - max_lines),
            Style::default()
                .fg(Color::Rgb(120, 120, 140))
                .add_modifier(Modifier::DIM),
        ))));
    }
    lines
}

/// Short result note for a timeline row (`12 lines`, `3 items`, `ok`).
pub fn timeline_summary(output: &Value) -> Option<String> {
    match output {
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => {
            let lines = s.lines().count();
            Some(if lines > 1 {
                format!("{} lines", lines)
            } else {
                format!("{} chars", s.chars().count())
            })
        }
        Value::Array(a) if a.is_empty() => None,
        Value::Array(a) => Some(format!(
            "{} item{}",
            a.len(),
            if a.len() == 1 { "" } else { "s" }
        )),
        Value::Object(o) if o.is_empty() => None,
        Value::Object(o) => Some(format!(
            "{} field{}",
            o.len(),
            if o.len() == 1 { "" } else { "s" }
        )),
        Value::Null => None,
        other => Some(value_inline(other)),
    }
}

/// The most telling argument, rendered without the `key=` prefix.
pub fn arg_headline(arguments: &Value) -> String {
    let Value::Object(map) = arguments else {
        return truncate_inline(&value_inline(arguments), 40);
    };
    for key in HEADLINE_ARGS {
        if let Some(value) = map.get(*key) {
            let rendered = match value {
                Value::String(s) => s.clone(),
                other => value_inline(other),
            };
            if !rendered.trim().is_empty() {
                return truncate_inline(&rendered, 40);
            }
        }
    }
    // No headline argument: fall back to the first two key=value pairs.
    let fallback: Vec<String> = map
        .iter()
        .take(2)
        .map(|(k, v)| format!("{}={}", k, value_inline(v)))
        .collect();
    truncate_inline(&fallback.join(", "), 40)
}

fn truncate_inline(s: &str, max_chars: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    let end = one_line
        .char_indices()
        .take_while(|(i, _)| *i < max_chars.saturating_sub(1))
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}\u{2026}", &one_line[..end])
}

/// `120ms`, `1.2s`, `12s`, `1m 05s`.
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1000)
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

    // ── timeline rows ──────────────────────────────────────

    #[test]
    fn timeline_row_carries_name_headline_summary_and_duration() {
        let line = render_timeline_row(
            "read_file",
            Some(&json!({"path": "src/main.rs", "limit": 100})),
            true,
            Some("12 lines"),
            None,
            120,
            false,
        );
        let text = flatten(&line);
        assert!(text.contains("\u{251c}"), "missing timeline branch: {text}");
        assert!(text.contains("\u{2713}"), "missing success icon: {text}");
        assert!(text.contains("read_file"), "{text}");
        // The headline argument, without the `path=` noise.
        assert!(text.contains("src/main.rs"), "{text}");
        assert!(
            !text.contains("path="),
            "headline should drop the key: {text}"
        );
        assert!(text.contains("12 lines"), "{text}");
        assert!(text.contains("120ms"), "{text}");
    }

    #[test]
    fn timeline_row_error_uses_cross_and_diff_counts() {
        let line = render_timeline_row(
            "edit_file",
            Some(&json!({"path": "src/lib.rs"})),
            false,
            None,
            Some((4, 2)),
            1_250,
            true,
        );
        let text = flatten(&line);
        assert!(text.contains("\u{2717}"), "{text}");
        assert!(text.contains("+4 \u{2212}2"), "diff counts missing: {text}");
        assert!(text.contains("truncated"), "{text}");
        assert!(text.contains("1.2s"), "{text}");
    }

    #[test]
    fn timeline_row_stays_within_the_terminal_width() {
        let long = "x".repeat(200);
        let line = render_timeline_row(
            "run_command",
            Some(&json!({"command": long})),
            true,
            Some("3 lines"),
            None,
            90_000,
            false,
        );
        let width = line.width();
        assert!(width <= 96, "row overflowed: {width}");
        assert!(
            flatten(&line).contains("\u{2026}"),
            "long argument should be elided"
        );
    }

    #[test]
    fn timeline_row_without_arguments_or_summary_is_just_the_name() {
        let line = render_timeline_row("todowrite", None, true, None, None, 5, false);
        let text = flatten(&line);
        assert!(text.contains("todowrite"), "{text}");
        assert!(text.contains("5ms"), "{text}");
    }

    /// An unmeasured call reports `duration_ms = 0`; printing `0ms` reads as
    /// "instant" instead of "unknown", so the column is dropped.
    #[test]
    fn timeline_row_omits_unknown_duration() {
        let line = render_timeline_row(
            "read_file",
            Some(&json!({"path": "a.rs"})),
            true,
            None,
            None,
            0,
            false,
        );
        let text = flatten(&line).trim_end().to_string();
        assert!(!text.contains("0ms"), "{text}");
        assert!(text.ends_with("a.rs"), "{text}");
    }

    #[test]
    fn arg_headline_prefers_the_most_telling_argument() {
        assert_eq!(
            arg_headline(&json!({"limit": 10, "path": "a/b.rs"})),
            "a/b.rs"
        );
        assert_eq!(
            arg_headline(&json!({"tool": "x", "command": "cargo test"})),
            "cargo test"
        );
        // Unknown shape falls back to key=value pairs.
        assert!(arg_headline(&json!({"foo": "bar"})).contains("foo=\"bar\""));
        // Multi-line arguments collapse to one row.
        assert_eq!(arg_headline(&json!({"command": "a\nb"})), "a b");
    }

    #[test]
    fn timeline_summary_describes_the_result_shape() {
        assert_eq!(
            timeline_summary(&Value::String("a\nb".into())).as_deref(),
            Some("2 lines")
        );
        assert_eq!(
            timeline_summary(&json!([1, 2, 3])).as_deref(),
            Some("3 items")
        );
        assert_eq!(timeline_summary(&json!({})), None);
        assert_eq!(timeline_summary(&Value::String("  ".into())), None);
    }

    #[test]
    fn error_detail_lines_are_capped_and_guided() {
        let message: String = (1..=8)
            .map(|i| format!("error line {i}\n"))
            .collect::<String>()
            .trim_end()
            .to_string();
        let lines = error_detail_lines(&message, 4);
        assert_eq!(lines.len(), 5, "4 lines + elision notice");
        assert!(flatten(&lines[0]).starts_with(TIMELINE_DETAIL));
        assert!(flatten(&lines[4]).contains("4 more line(s)"));
    }

    #[test]
    fn timeline_detail_keeps_the_guide_and_the_original_content() {
        let line = timeline_detail(&Line::from(Span::raw("nested output")));
        let text = flatten(&line);
        assert!(text.starts_with(TIMELINE_DETAIL), "{text}");
        assert!(text.contains("nested output"), "{text}");
    }

    #[test]
    fn format_duration_scales_with_magnitude() {
        assert_eq!(format_duration(120), "120ms");
        assert_eq!(format_duration(1_250), "1.2s");
        assert_eq!(format_duration(65_000), "1m 05s");
    }

    #[test]
    fn value_inline_truncates_arrays() {
        let s = value_inline(&json!([1, 2, 3, 4, 5]));
        assert!(s.contains("+2"));
    }
}
