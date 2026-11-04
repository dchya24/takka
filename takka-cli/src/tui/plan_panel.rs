//! TUI plan panel widget.
//!
//! Renders a dedicated panel for plan display with:
//! - Goal header
//! - Progress bar (completed / total / failed)
//! - Current step description with status icon
//!
//! Note: This module requires a working TUI build (ratatui + crossterm).
//! The TUI has pre-existing build errors (`crossterm` in dev-dependencies
//! but not in `[dependencies]`). Once that's fixed, import this in
//! `app.rs` and `ui.rs`.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame,
};

/// Plan state rendered by the TUI plan panel.
pub struct PlanPanelState {
    pub goal: String,
    pub current_step: String,
    pub step_status: String,
    pub steps_total: usize,
    pub steps_completed: usize,
    pub steps_failed: usize,
    pub steps_pending: usize,
    pub is_active: bool,
}

impl Default for PlanPanelState {
    fn default() -> Self {
        Self {
            goal: String::new(),
            current_step: String::new(),
            step_status: String::new(),
            steps_total: 0,
            steps_completed: 0,
            steps_failed: 0,
            steps_pending: 0,
            is_active: false,
        }
    }
}

impl PlanPanelState {
    /// Update state from a PlanProgress event payload.
    pub fn update(
        &mut self,
        goal: String,
        current_step: String,
        step_status: String,
        steps_total: usize,
        steps_completed: usize,
        steps_failed: usize,
        steps_pending: usize,
    ) {
        self.goal = goal;
        self.current_step = current_step;
        self.step_status = step_status;
        self.steps_total = steps_total;
        self.steps_completed = steps_completed;
        self.steps_failed = steps_failed;
        self.steps_pending = steps_pending;
        self.is_active = true;
    }

    /// Reset to inactive state.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Render the plan panel into a region of the TUI.
pub fn render_plan_panel(frame: &mut Frame, area: Rect, state: &PlanPanelState) {
    let block = Block::default()
        .title(format!(" 🗺️  Plan: {} ", state.goal))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(20, 20, 30)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout: progress gauge + step description
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // blank
            Constraint::Length(3), // progress gauge
            Constraint::Length(1), // blank
            Constraint::Length(1), // step description
            Constraint::Min(0),    // remaining
        ])
        .split(inner);

    // Progress gauge
    let pct = if state.steps_total > 0 {
        (state.steps_completed as f64 / state.steps_total as f64) * 100.0
    } else {
        0.0
    };

    let gauge_label = format!(
        "{}/{}  {}",
        state.steps_completed, state.steps_total,
        if state.steps_failed > 0 {
            format!("({} failed)", state.steps_failed)
        } else {
            String::new()
        }
    );

    let gauge = Gauge::default()
        .gauge_style(
            if state.steps_failed > 0 {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            },
        )
        .percent(pct as u16)
        .label(gauge_label);
    frame.render_widget(gauge, chunks[1]);

    // Step description
    let icon = match state.step_status.as_str() {
        "in_progress" => "▶",
        "completed" => "✅",
        "failed" => "❌",
        _ => "⏳",
    };

    let step_line = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" {}  ", icon),
            Style::default().fg(match state.step_status.as_str() {
                "completed" => Color::Green,
                "failed" => Color::Red,
                "in_progress" => Color::Yellow,
                _ => Color::Gray,
            }),
        ),
        Span::raw(&state.current_step),
    ]))
    .wrap(Wrap { trim: true });
    frame.render_widget(step_line, chunks[3]);
}
