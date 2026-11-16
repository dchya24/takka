use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};

pub(crate) fn render_event(event: &takka_core::Event, timeline: &mut ToolTimeline, detailed: bool) {
    use crate::widgets::{components, diff as diff_widget, inline, tool_call};

    // Tools render as a timeline: one row per finished call (icon, name, the
    // argument that says what it touched, timing), with streamed output,
    // errors and diffs hanging off a `│` guide beneath it. While a tool is
    // running nothing is printed — the transient status line already names it
    // and ticks its own clock, so a finished row replaces that noise.
    //
    // Special case: edit_file / write_file results carry a `diff` field
    // (unified-diff string) and `lines_added` / `lines_removed` counts.
    // We render those through `widgets::diff` so the user sees a real
    // colored diff inline instead of the raw JSON blob.
    pub(crate) const MAX_TOOL_OUTPUT_LINES: usize = 12;
    pub(crate) const MAX_ERROR_LINES: usize = 4;
    match event {
        takka_core::Event::ToolCall {
            tool_name,
            arguments,
        } => {
            timeline.on_call(tool_name, arguments);
        }
        takka_core::Event::ToolStarted {
            tool_call_id,
            tool_name,
            arguments,
            ..
        } => {
            timeline.on_started(tool_call_id, tool_name, arguments);
        }
        takka_core::Event::ToolDelta {
            delta,
            tool_name,
            tool_call_id,
            ..
        } => {
            if !detailed {
                // Compact: the completion row alone represents the tool.
                return;
            }
            // First delta: give the streamed lines a parent row to hang from.
            if timeline.mark_live(tool_call_id) {
                let arguments = timeline.peek_arguments(tool_call_id);
                inline::print_line(&tool_call::render_running_row(
                    tool_name,
                    arguments.as_ref(),
                ));
            }
            for line in render_tool_delta(tool_name, delta) {
                inline::print_line(&tool_call::timeline_detail(&line));
            }
        }
        takka_core::Event::ToolOutput {
            tool_name,
            output,
            tool_call_id,
            duration_ms,
            success,
            truncated,
            ..
        } => {
            // Heuristic: the orchestrator records denied/skipped outcomes as
            // plain string output with these prefixes, in which case `success`
            // is still true but the run did not do what was asked.
            let body = match output {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let denied = body.starts_with("Blocked:") || body.starts_with("Skipped:");
            let is_error = !*success || denied || body.starts_with("Tool error");

            // Structured fields ride along as JSON in the string body.
            let parsed = if is_error {
                None
            } else {
                serde_json::from_str::<serde_json::Value>(&body).ok()
            };
            let diff_text = parsed
                .as_ref()
                .and_then(|v| v.get("diff"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let diff_counts = parsed.as_ref().and_then(|v| {
                let added = v.get("lines_added")?.as_u64()? as u32;
                let removed = v.get("lines_removed")?.as_u64()? as u32;
                Some((added, removed))
            });

            let arguments = timeline.arguments_for(tool_call_id, tool_name);
            let summary = if is_error {
                None
            } else {
                tool_call::timeline_summary(output)
            };

            if !detailed {
                inline::print_line(&tool_call::render_timeline_row(
                    tool_name,
                    arguments.as_ref(),
                    !is_error,
                    summary.as_deref(),
                    diff_counts,
                    *duration_ms,
                    *truncated,
                ));
                // Drop the live marker too: a later `ToolDelta` for the same
                // call must not reopen a row in compact mode.
                timeline.take_live(tool_call_id);
                return;
            }

            if timeline.take_live(tool_call_id) {
                // The running row is already on screen with the streamed
                // output beneath it — just close it out.
                let mut outcome = tool_call::timeline_summary(output);
                if *truncated {
                    let note = "truncated".to_string();
                    outcome = Some(match outcome {
                        Some(existing) => format!("{existing} \u{b7} {note}"),
                        None => note,
                    });
                }
                inline::print_line(&tool_call::render_outcome_detail(
                    !is_error,
                    outcome.as_deref(),
                    *duration_ms,
                ));
            } else {
                inline::print_line(&tool_call::render_timeline_row(
                    tool_name,
                    arguments.as_ref(),
                    !is_error,
                    summary.as_deref(),
                    diff_counts,
                    *duration_ms,
                    *truncated,
                ));
            }

            // Errors always show their message — the user must be able to act
            // on them without flipping a flag.
            if is_error {
                for line in tool_call::error_detail_lines(&body, MAX_ERROR_LINES) {
                    inline::print_line(&line);
                }
            }

            // Diffs render inline (no border) beneath the row.
            if let Some(diff) = diff_text {
                inline::print_line(&tool_call::timeline_detail(&diff_widget::summary_line(
                    diff,
                )));
                let diff_lines = diff_widget::render(diff);
                let shown = diff_lines.len().min(MAX_TOOL_OUTPUT_LINES);
                for line in &diff_lines[..shown] {
                    inline::print_line(&tool_call::timeline_detail(line));
                }
                if diff_lines.len() > shown {
                    inline::print_line(&tool_call::error_detail_lines(
                        &format!(
                            "\u{2026} {} more diff line(s) hidden",
                            diff_lines.len() - shown
                        ),
                        1,
                    )
                    .into_iter()
                    .next()
                    .unwrap_or_default());
                }
            }
        }
        takka_core::Event::Thinking { content }
            // Display the LLM's thinking/reasoning before tool execution
            // with DIM styling so it's visually distinct from the final response.
            if !content.trim().is_empty() => {
                inline::print_line(&components::thinking_header(true));
                inline::print_lines(&thinking_block_lines(content, THINKING_PREVIEW_LINES));
                inline::print_line(&components::thinking_header(false));
            }
        takka_core::Event::SteerQueued { content } => {
            // Story 2.1: the steering hand-over is visible — ack above
            // the (paused) input row, part of the run's scrollback.
            let dim = RStyle::default().fg(RColor::Rgb(110, 110, 140));
            inline::print_line(&RLine::from(vec![
                RSpan::styled(
                    "  ↳ steered in (applies at the next tool boundary): ".to_string(),
                    dim,
                ),
                RSpan::styled(
                    format!("\"{content}\""),
                    RStyle::default()
                        .fg(RColor::Rgb(120, 180, 140)),
                ),
            ]));
        }
        takka_core::Event::Steered { content } => {
            // The boundary drain applied the steer: hand-over marker.
            let dim = RStyle::default().fg(RColor::Rgb(110, 110, 140));
            inline::print_line(&RLine::from(vec![
                RSpan::styled("  ↳ applied at the tool boundary: ".to_string(), dim),
                RSpan::styled(
                    format!("\"{content}\""),
                    RStyle::default().fg(RColor::Rgb(120, 180, 140)),
                ),
            ]));
        }
        takka_core::Event::Error { message } => {
            inline::print_line(&components::error_badge(message));
        }
        // H-06: the run stopped on budget, not on failure. Name the cause and
        // the way out instead of letting the turn end silently.
        takka_core::Event::CostLimitExceeded {
            max_cost_usd,
            spent_usd,
        } => {
            inline::print_line(&components::warning_badge(&crate::widgets::budget::stop_notice(
                *max_cost_usd,
                *spent_usd,
            )));
        }
        takka_core::Event::System { message } => {
            inline::print_line(&components::info_badge(message));
        }
        // ConfirmationRequest / Completed are surfaced via other
        // channels (the confirmation prompt, the final markdown).
        _ => {}
    }
}

/// Per-run state for the inline tool timeline (see [`render_event`]).
///
/// `ToolOutput` carries no arguments, so the arguments seen at
/// `ToolStarted` (keyed by call id) or `ToolCall` are remembered here — that
/// is what lets a finished row say *what* the tool touched, not just its name.
#[derive(Default)]
pub(crate) struct ToolTimeline {
    pub(crate) calls: std::collections::HashMap<String, serde_json::Value>,
    /// Calls whose streamed row has already been printed, so the completion
    /// prints a nested outcome under it instead of a second branch row.
    pub(crate) live: std::collections::HashSet<String>,
    /// `ToolCall` has no id; remember it for outputs that arrive without a
    /// matching `ToolStarted`.
    pub(crate) last_call: Option<(String, serde_json::Value)>,
}

impl ToolTimeline {
    pub(crate) fn on_call(&mut self, tool_name: &str, arguments: &serde_json::Value) {
        self.last_call = Some((tool_name.to_string(), arguments.clone()));
    }

    pub(crate) fn on_started(
        &mut self,
        call_id: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) {
        self.calls.insert(call_id.to_string(), arguments.clone());
        self.last_call = Some((tool_name.to_string(), arguments.clone()));
    }

    /// Mark a call as having a live (streaming) row. Returns `true` the first
    /// time, i.e. when the caller should print the running row.
    pub(crate) fn mark_live(&mut self, call_id: &str) -> bool {
        self.live.insert(call_id.to_string())
    }

    /// `true` when this call already owns a live row that the outcome should
    /// hang beneath; the marker is consumed.
    pub(crate) fn take_live(&mut self, call_id: &str) -> bool {
        self.live.remove(call_id)
    }

    /// Arguments for a call without consuming them.
    pub(crate) fn peek_arguments(&mut self, call_id: &str) -> Option<serde_json::Value> {
        self.calls.get(call_id).cloned()
    }

    /// Arguments for a call that just finished; the entry is consumed so a
    /// long run does not accumulate state.
    pub(crate) fn arguments_for(
        &mut self,
        call_id: &str,
        tool_name: &str,
    ) -> Option<serde_json::Value> {
        if let Some(arguments) = self.calls.remove(call_id) {
            return Some(arguments);
        }
        match &self.last_call {
            Some((name, arguments)) if name == tool_name => Some(arguments.clone()),
            _ => None,
        }
    }
}

/// Longest reasoning block the inline renderer prints before eliding the
/// rest — the model's pre-tool reasoning can run to paragraphs, and the real
/// answer must not be pushed off screen.
pub(crate) const THINKING_PREVIEW_LINES: usize = 12;

/// Format a `Thinking` event body as indented DIM preview lines, capped at
/// `max_lines` (with an `… N more line(s)` notice). Pure — no terminal access,
/// so the output can be unit-tested.
pub(crate) fn thinking_block_lines(content: &str, max_lines: usize) -> Vec<RLine<'static>> {
    let style = RStyle::default()
        .fg(RColor::Indexed(242))
        .add_modifier(RModifier::DIM);
    let hidden_style = RStyle::default()
        .fg(RColor::Indexed(240))
        .add_modifier(RModifier::DIM | RModifier::ITALIC);

    let body: Vec<&str> = content
        .lines()
        .map(str::trim_end)
        .skip_while(|l| l.trim().is_empty())
        .collect();
    // A trailing blank line in the model's block shouldn't cost a preview row.
    let body_len = body
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut lines: Vec<RLine<'static>> = body
        .iter()
        .take(body_len.min(max_lines))
        .map(|line| RLine::from(RSpan::styled(format!("  {}", line), style)))
        .collect();

    if body_len > max_lines {
        lines.push(RLine::from(RSpan::styled(
            format!("  … {} more line(s) hidden", body_len - max_lines),
            hidden_style,
        )));
    }

    lines
}

/// Render a live tool-output delta as indented DIM lines. Pure — no
/// terminal access — so it can be unit-tested without capturing stdout.
pub(crate) fn render_tool_delta(_tool_name: &str, delta: &str) -> Vec<RLine<'static>> {
    let mut lines = Vec::new();
    for line in delta.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // No extra indent: the caller prefixes these with the timeline guide.
        lines.push(RLine::from(RSpan::styled(
            line.to_string(),
            RStyle::default()
                .fg(RColor::Indexed(244))
                .add_modifier(RModifier::DIM),
        )));
    }
    lines
}

#[cfg(test)]
mod thinking_preview_tests {
    use super::*;

    fn text(line: &RLine<'_>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn short_blocks_are_printed_whole() {
        let lines = thinking_block_lines("Plan:\n1. read main.rs\n2. fix bug", 12);
        assert_eq!(lines.len(), 3);
        assert!(text(&lines[0]).contains("Plan:"));
        assert!(text(&lines[2]).contains("fix bug"));
    }

    #[test]
    fn long_blocks_are_capped_with_a_notice() {
        let content: String = (1..=20)
            .map(|i| format!("step {i}\n"))
            .collect::<String>()
            .trim_end()
            .to_string();
        let lines = thinking_block_lines(&content, 12);
        assert_eq!(lines.len(), 13, "12 preview lines + notice");
        let notice = text(lines.last().unwrap());
        assert!(notice.contains("8 more line(s) hidden"), "{notice}");
    }

    #[test]
    fn leading_and_trailing_blank_lines_do_not_cost_preview_rows() {
        let lines = thinking_block_lines("\n\n  actual thought  \n\n", 12);
        assert_eq!(lines.len(), 1, "blank padding must be trimmed");
        assert!(text(&lines[0]).contains("actual thought"));
    }

    #[test]
    fn empty_or_whitespace_only_content_renders_nothing() {
        assert!(thinking_block_lines("", 12).is_empty());
        assert!(thinking_block_lines("   \n\t\n", 12).is_empty());
    }
}

#[cfg(test)]
mod render_tool_delta_tests {
    use super::*;

    #[test]
    fn skips_blank_lines() {
        let lines = render_tool_delta("run_command", "a\n\nb\n");
        let rendered: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(rendered.len(), 2, "blank lines should be skipped");
        assert!(rendered[0].contains("a"));
        assert!(rendered[1].contains("b"));
    }

    #[test]
    fn leaves_indentation_to_the_caller() {
        // The renderer wraps these in `tool_call::timeline_detail`, which adds
        // the guide prefix — a local indent here would double up.
        let lines = render_tool_delta("run_command", "cargo: compiling\ncargo: done");
        assert_eq!(lines.len(), 2);
        for l in &lines {
            assert!(
                !l.to_string().starts_with(' '),
                "delta lines must not self-indent"
            );
        }
    }

    #[test]
    fn empty_delta_produces_no_lines() {
        let lines = render_tool_delta("run_command", "");
        assert!(lines.is_empty());
    }
}
