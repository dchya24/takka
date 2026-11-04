//! UI rendering for TUI

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};

use super::app::{App, MessageRole};
use super::dropdown::DropdownType;
use super::input::{render_input, render_placeholder};
use crate::widgets::markdown::{role_prefix, MarkdownContent};
use crate::widgets::{diff as diff_widget, spinner, tool_call};

/// Padding configuration
const PADDING_HORIZONTAL: u16 = 2;
const PADDING_VERTICAL: u16 = 1;

/// Draw the entire UI
pub fn draw(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    // Main container with padding
    let padded_area = Rect {
        x: size.x + PADDING_HORIZONTAL,
        y: size.y + PADDING_VERTICAL,
        width: size.width.saturating_sub(PADDING_HORIZONTAL * 2),
        height: size.height.saturating_sub(PADDING_VERTICAL * 2),
    };

    // Background
    let bg_block = Block::default().style(Style::default().bg(Color::Rgb(20, 20, 30)));
    frame.render_widget(bg_block, size);

    // Main layout: header, messages, input
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(10),   // Messages
            Constraint::Length(3), // Progress (when loading)
            Constraint::Length(3), // Input
        ])
        .split(padded_area);

    // Draw components
    draw_header(frame, app, chunks[0]);
    draw_messages(frame, app, chunks[1]);
    draw_progress(frame, app, chunks[2]);
    draw_input(frame, app, chunks[3]);

    // Draw dropdown overlay if active
    if app.dropdown.is_some() {
        draw_dropdown(frame, app, chunks[3]);
    }

    // Draw session view overlay if active
    if app.session_view.is_some() {
        draw_session_view(frame, app, chunks[1]);
    }
}

/// Draw header bar
fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let (provider, model, _) = app.model_info();
    let session_id_short = &app.session.id[4..app.session.id.len().min(16)];
    let context = app.context_indicators();

    let mut spans = vec![
        Span::styled(
            " 🤖 Takka ",
            Style::default()
                .fg(Color::Rgb(46, 204, 113))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {} / {} ", provider, model),
            Style::default().fg(Color::Rgb(180, 180, 180)),
        ),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" 💬{} ", app.stats.messages_sent),
            Style::default().fg(Color::Rgb(135, 206, 250)),
        ),
        Span::styled(
            format!(
                " 📊{}↑/{}↓ ",
                app.stats.format_tokens(app.stats.tokens_input),
                app.stats.format_tokens(app.stats.tokens_output)
            ),
            Style::default().fg(Color::Rgb(186, 85, 211)),
        ),
    ];

    // Context indicator (G-11): AGENT.md / memory.md
    if !context.is_empty() {
        spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            context,
            Style::default().fg(Color::Rgb(241, 196, 15)),
        ));
    }

    spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        format!(" {} ", session_id_short),
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(
        " /help ",
        Style::default().fg(Color::Rgb(241, 196, 15)),
    ));

    let header = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(60, 60, 80)))
            .style(Style::default().bg(Color::Rgb(30, 30, 45))),
    );

    frame.render_widget(header, area);
}

/// Draw messages area
fn draw_messages(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner_area = area.inner(Margin::new(1, 1));

    // Build message lines
    let mut all_lines: Vec<Line> = Vec::new();

    for message in &app.messages {
        // Add spacing between messages
        if !all_lines.is_empty() {
            all_lines.push(Line::default());
        }

        // Tool messages render through the dedicated widgets, not as
        // markdown. They carry a JSON envelope in `content`.
        match message.role {
            MessageRole::Tool => {
                if let Some((name, args)) = parse_tool_call_payload(&message.content) {
                    let lines = tool_call::render_call(&name, &args);
                    all_lines.extend(lines);
                } else {
                    all_lines.push(Line::from(Span::styled(
                        "(malformed tool call event)",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                continue;
            }
            MessageRole::ToolActivity => {
                // Live streaming output from a tool: strip the "[name]"
                // prefix and render the body as indented DIM lines.
                let body = message
                    .content
                    .split_once('\n')
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_else(|| message.content.clone());
                for line in body.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    all_lines.push(Line::from(Span::styled(
                        format!("    {}", line),
                        Style::default()
                            .fg(Color::Indexed(244))
                            .add_modifier(Modifier::DIM),
                    )));
                }
                continue;
            }
            MessageRole::ToolResult | MessageRole::ToolError => {
                if let Some((name, output)) = parse_tool_result_payload(&message.content) {
                    let is_error = matches!(message.role, MessageRole::ToolError);
                    let lines = tool_call::render_result(&name, &output, is_error, 12, false);
                    all_lines.extend(lines);

                    // If the result carries a unified diff (edit_file /
                    // write_file), render it inline through the diff
                    // widget so the user sees real colored hunks.
                    if !is_error {
                        if let Some(diff_text) = extract_diff_string(&output) {
                            all_lines.push(diff_widget::summary_line(&diff_text));
                            let diff_lines = diff_widget::render(&diff_text);
                            let max_diff_lines = 40;
                            if diff_lines.len() > max_diff_lines {
                                all_lines.extend(diff_lines.into_iter().take(max_diff_lines));
                                all_lines.push(Line::from(Span::styled(
                                    "    … diff truncated",
                                    Style::default()
                                        .fg(Color::DarkGray)
                                        .add_modifier(Modifier::DIM),
                                )));
                            } else {
                                all_lines.extend(diff_lines);
                            }
                        }
                    }
                } else {
                    all_lines.push(Line::from(Span::styled(
                        "(malformed tool result event)",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                continue;
            }
            _ => {}
        }

        // Role header with timestamp
        let (role_span, content_style) = match message.role {
            MessageRole::User => role_prefix("user"),
            MessageRole::Assistant => role_prefix("assistant"),
            MessageRole::System => role_prefix("system"),
            MessageRole::Error => role_prefix("error"),
            // Tool variants handled above with `continue`; unreachable here.
            _ => role_prefix("system"),
        };

        let time_str = message.timestamp.format("%H:%M").to_string();
        all_lines.push(Line::from(vec![
            role_span,
            Span::styled(
                format!("  {}", time_str),
                Style::default().fg(Color::DarkGray),
            ),
        ]));

        // Parse and render markdown content
        let md_content = MarkdownContent::parse(&message.content);
        for line in md_content.lines {
            // Apply content style to each span if needed
            let styled_line = if content_style != Style::default() {
                Line::from(
                    line.spans
                        .into_iter()
                        .map(|span| {
                            if span.style == Style::default() {
                                Span::styled(span.content, content_style)
                            } else {
                                span
                            }
                        })
                        .collect::<Vec<_>>(),
                )
            } else {
                line
            };
            all_lines.push(styled_line);
        }
    }

    // Add current streaming response if any
    if !app.current_response.is_empty() {
        if !all_lines.is_empty() {
            all_lines.push(Line::default());
        }

        let (role_span, _) = role_prefix("assistant");
        all_lines.push(Line::from(vec![
            role_span,
            Span::styled(
                "  streaming...",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));

        let md_content = MarkdownContent::parse_partial(&app.current_response);
        for line in md_content.lines {
            all_lines.push(line);
        }
    }

    // Calculate scroll
    let visible_height = inner_area.height as usize;
    let total_lines = all_lines.len();

    // Auto-scroll to bottom when new content arrives
    let max_scroll = total_lines.saturating_sub(visible_height);
    if app.scroll_offset > max_scroll {
        app.scroll_offset = max_scroll;
    }

    // Create paragraph with scroll
    let messages_widget = Paragraph::new(all_lines.clone())
        .scroll((app.scroll_offset as u16, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(60, 60, 80)))
                .title(Span::styled(
                    " Messages ",
                    Style::default()
                        .fg(Color::Rgb(180, 180, 180))
                        .add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(Color::Rgb(25, 25, 35))),
        );

    frame.render_widget(messages_widget, area);

    // Scrollbar
    if total_lines > visible_height {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"))
            .track_symbol(Some("│"))
            .thumb_symbol("█");

        let mut scrollbar_state = ScrollbarState::new(total_lines)
            .position(app.scroll_offset)
            .viewport_content_length(visible_height);

        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin::new(0, 1)),
            &mut scrollbar_state,
        );
    }
}

/// Draw progress indicator
fn draw_progress(frame: &mut Frame, app: &App, area: Rect) {
    if !app.is_loading {
        // Empty space when not loading
        let empty = Block::default().style(Style::default().bg(Color::Rgb(20, 20, 30)));
        frame.render_widget(empty, area);
        return;
    }

    // Reuse the shared spinner widget so the TUI matches inline mode.
    let bar_width = area.width.saturating_sub(4) as usize;
    let progress_widget = Paragraph::new(vec![
        spinner::spinner_line(&app.progress),
        spinner::progress_bar_line(&app.progress, bar_width),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(52, 152, 219)))
            .style(Style::default().bg(Color::Rgb(25, 30, 40))),
    );

    frame.render_widget(progress_widget, area);
}

/// Draw input area
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let input_line = if app.input.is_empty() {
        render_placeholder()
    } else {
        render_input(&app.input, app.cursor_pos)
    };

    let input_widget = Paragraph::new(input_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if app.is_loading {
                Color::DarkGray
            } else {
                Color::Rgb(46, 204, 113)
            }))
            .title(Span::styled(
                if app.is_loading {
                    " Input (waiting...) ".to_string()
                } else if let Some(ref img) = app.image_attachment {
                    format!(" Input 📷 {} ", img)
                } else {
                    " Input ".to_string()
                },
                Style::default()
                    .fg(if app.is_loading {
                        Color::DarkGray
                    } else {
                        Color::Rgb(46, 204, 113)
                    })
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(Color::Rgb(30, 30, 45))),
    );

    frame.render_widget(input_widget, area);
}

/// Draw dropdown overlay
fn draw_dropdown(frame: &mut Frame, app: &App, input_area: Rect) {
    let dropdown = match &app.dropdown {
        Some(d) => d,
        None => return,
    };

    if dropdown.is_empty() {
        return;
    }

    // Calculate dropdown position (above input)
    let visible_items = dropdown.visible_items();
    let dropdown_height = (visible_items.len() + 2).min(10) as u16;

    let dropdown_area = Rect {
        x: input_area.x + 1,
        y: input_area.y.saturating_sub(dropdown_height),
        width: input_area.width.saturating_sub(2).min(50),
        height: dropdown_height,
    };

    // Clear area behind dropdown
    frame.render_widget(Clear, dropdown_area);

    // Build list items
    let items: Vec<ListItem> = visible_items
        .iter()
        .map(|(i, item, selected)| {
            let style = if *selected {
                Style::default()
                    .bg(Color::Rgb(52, 152, 219))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(200, 200, 200))
            };

            let icon = match dropdown.dropdown_type {
                DropdownType::Command => "⌘ ",
                DropdownType::File => {
                    if item.ends_with('/') {
                        "📁 "
                    } else {
                        "📄 "
                    }
                }
                DropdownType::Model => "🤖 ",
                DropdownType::Skill => "⚡ ",
            };

            let mut spans = vec![
                Span::styled(icon, style),
                Span::styled(item.to_string(), style),
            ];

            // Add description for commands
            if let Some(desc) = dropdown.get_description(*i) {
                spans.push(Span::styled(
                    format!("  {}", desc),
                    if *selected {
                        Style::default()
                            .bg(Color::Rgb(52, 152, 219))
                            .fg(Color::Rgb(200, 200, 200))
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = format!(" {} {} ", dropdown.icon(), dropdown.title());
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(100, 100, 120)))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::Rgb(241, 196, 15))
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(Color::Rgb(35, 35, 50))),
    );

    frame.render_widget(list, dropdown_area);
}

// ── Tool event payload decoding ─────────────────────────────
//
// AppMessage::ToolCall and AppMessage::ToolResult package the structured
// payload as a JSON string in `Message::content` so we can keep
// `Message` simple. The TUI message log decodes it back here.

fn parse_tool_call_payload(content: &str) -> Option<(String, serde_json::Value)> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let arguments = v.get("arguments")?.clone();
    Some((name, arguments))
}

fn parse_tool_result_payload(content: &str) -> Option<(String, serde_json::Value)> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let output = v.get("output")?.clone();
    Some((name, output))
}

/// Extract a unified-diff string from a tool result. The orchestrator
/// emits ToolOutput as `Value::String(json_string)` so we have to parse
/// twice: once to lift the inner JSON, once to look up the `diff` field.
fn extract_diff_string(output: &serde_json::Value) -> Option<String> {
    let body = output.as_str()?;
    let inner: serde_json::Value = serde_json::from_str(body).ok()?;
    let diff = inner.get("diff")?.as_str()?;
    if diff.is_empty() {
        None
    } else {
        Some(diff.to_string())
    }
}

/// Draw session list view overlay
fn draw_session_view(frame: &mut Frame, app: &App, area: Rect) {
    let view = match &app.session_view {
        Some(v) => v,
        None => return,
    };

    // Center the overlay on screen
    let popup_width = area.width.min(80);
    let popup_height = area.height.min(20);
    let popup_x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    // Clear the area behind
    frame.render_widget(Clear, popup_area);

    // Build session list items
    let mut items: Vec<ListItem> = Vec::new();

    if view.summaries.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  No sessions found.",
            Style::default().fg(Color::DarkGray),
        ))));
    } else {
        for (i, s) in view.summaries.iter().enumerate().take(15) {
            let is_selected = i == view.selected;
            let time = crate::session::format_relative_time(&s.updated_at);
            let title = if s.title.is_empty() {
                "Untitled"
            } else {
                &s.title
            };

            let style = if is_selected {
                Style::default()
                    .bg(Color::Rgb(52, 152, 219))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(200, 200, 200))
            };

            let line = Line::from(vec![
                Span::styled(format!(" {:2}. ", i + 1), style),
                Span::styled(title.to_string(), style),
                Span::styled(
                    format!("  {} msgs", s.message_count),
                    if is_selected {
                        style
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    format!("  {}", time),
                    if is_selected {
                        style
                    } else {
                        Style::default().fg(Color::Rgb(135, 206, 250))
                    },
                ),
            ]);
            items.push(ListItem::new(line));
        }
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(100, 100, 140)))
            .title(Span::styled(
                format!(" Sessions ({}) ", view.summaries.len()),
                Style::default()
                    .fg(Color::Rgb(241, 196, 15))
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(Color::Rgb(35, 35, 50))),
    );

    frame.render_widget(list, popup_area);

    // Footer with key hints
    let footer_area = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + popup_area.height - 1,
        width: popup_area.width - 2,
        height: 1,
    };
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        " ↑/↓ Navigate  Enter Resume  Esc Close ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )]));
    frame.render_widget(footer, footer_area);
}
