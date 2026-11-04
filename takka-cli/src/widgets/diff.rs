//! Minimal unified-diff renderer.
//!
//! Takes the kind of `--- a/file` / `+++ b/file` + `@@` hunk + `+`/`-`/` `
//! lines that tools like `git diff` or our own edit-preview emit, and turns
//! them into styled `Line`s. No diff *computation* here — that's the
//! caller's job (or a library like `similar`).
//!
//! Used by edit-preview tool calls and any inline change summary.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Render a unified-diff string into styled lines.
///
/// Recognized prefixes:
///   `--- ` / `+++ `  file headers       (cyan, dim)
///   `@@ `            hunk header        (magenta, bold)
///   `+`              addition           (green)
///   `-`              deletion           (red)
///   anything else    context            (default)
pub fn render(diff: &str) -> Vec<Line<'static>> {
    diff.lines().map(render_line).collect()
}

fn render_line(line: &str) -> Line<'static> {
    if line.starts_with("--- ") || line.starts_with("+++ ") {
        Line::from(Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::Rgb(52, 152, 219))
                .add_modifier(Modifier::DIM | Modifier::BOLD),
        ))
    } else if line.starts_with("@@") {
        Line::from(Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::Rgb(155, 89, 182))
                .add_modifier(Modifier::BOLD),
        ))
    } else if let Some(rest) = line.strip_prefix('+') {
        Line::from(vec![
            Span::styled(
                "+".to_string(),
                Style::default()
                    .fg(Color::Rgb(46, 204, 113))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                rest.to_string(),
                Style::default().fg(Color::Rgb(46, 204, 113)),
            ),
        ])
    } else if let Some(rest) = line.strip_prefix('-') {
        Line::from(vec![
            Span::styled(
                "-".to_string(),
                Style::default()
                    .fg(Color::Rgb(231, 76, 60))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                rest.to_string(),
                Style::default().fg(Color::Rgb(231, 76, 60)),
            ),
        ])
    } else {
        Line::from(Span::raw(line.to_string()))
    }
}

/// Compute and render an inline summary of additions/deletions:
/// `+12 −3  in 2 hunks`
pub fn summary_line(diff: &str) -> Line<'static> {
    let mut adds = 0usize;
    let mut dels = 0usize;
    let mut hunks = 0usize;
    for line in diff.lines() {
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            continue;
        }
        if line.starts_with("@@") {
            hunks += 1;
        } else if line.starts_with('+') {
            adds += 1;
        } else if line.starts_with('-') {
            dels += 1;
        }
    }

    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("+{}", adds),
            Style::default()
                .fg(Color::Rgb(46, 204, 113))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("−{}", dels),
            Style::default()
                .fg(Color::Rgb(231, 76, 60))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("in {} hunk{}", hunks, if hunks == 1 { "" } else { "s" }),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flatten(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    const SAMPLE: &str = "--- a/foo.rs
+++ b/foo.rs
@@ -1,3 +1,3 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
 }
";

    #[test]
    fn render_produces_one_line_per_input_line() {
        let lines = render(SAMPLE);
        assert_eq!(lines.len(), SAMPLE.lines().count());
    }

    #[test]
    fn render_styles_addition_and_deletion() {
        let lines = render(SAMPLE);
        let added = lines
            .iter()
            .find(|l| flatten(l).contains("println!(\"new\")"))
            .expect("addition line missing");
        // First span is the '+' marker, styled with green fg.
        assert_eq!(added.spans[0].content, "+");
        assert!(added.spans[0].style.fg.is_some());

        let removed = lines
            .iter()
            .find(|l| flatten(l).contains("println!(\"old\")"))
            .expect("deletion line missing");
        assert_eq!(removed.spans[0].content, "-");
        assert!(removed.spans[0].style.fg.is_some());
    }

    #[test]
    fn summary_counts_hunks_adds_dels() {
        let line = summary_line(SAMPLE);
        let text = flatten(&line);
        assert!(text.contains("+1"));
        assert!(text.contains("−1"));
        assert!(text.contains("1 hunk"));
    }
}
