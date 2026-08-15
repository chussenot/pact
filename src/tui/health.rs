//! Health: the doctor checks.
//!
//! Owned by pact-pyt.8, which demotes the repo-setup checks and promotes the
//! fleet-behaviour ones. Seeded by the split (pact-pyt.1) with today's doctor
//! pane, ported to the view contract.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use ratatui::crossterm::event::KeyCode;

use super::nav::View;
use super::App;
use crate::doctor;

#[derive(Default)]
pub struct State {
    /// `None` until Health has been visited at least once — it shells out, so
    /// it is not run on a screen nobody is looking at.
    pub report: Option<doctor::DoctorReport>,
}

pub fn refresh(app: &mut App) {
    app.health.report = Some(doctor::checks(&app.repo_root));
}

/// Nothing here is an entity yet. pact-pyt.8 makes the fleet-behaviour checks
/// link to the agent or path they are about.
pub fn on_enter(_app: &App) -> Option<View> {
    None
}

pub fn handle_key(_app: &mut App, _code: KeyCode) -> bool {
    false
}

pub fn row_at(_app: &App, _x: u16, _y: u16) -> Option<usize> {
    None
}

pub fn select(_app: &mut App, _index: usize) {}

pub fn help() -> &'static str {
    "r: re-run checks"
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(report) = &app.health.report else {
        frame.render_widget(
            Paragraph::new("press r to run health checks")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    };

    let lines: Vec<Line> = report
        .checks
        .iter()
        .map(|c| {
            let (symbol, style) = match (c.ok, c.warn) {
                (false, _) => ("✗", Style::default().fg(Color::Red)),
                (true, true) => ("!", Style::default().fg(Color::Yellow)),
                (true, false) => ("✓", Style::default().fg(Color::Green)),
            };
            Line::from(Span::styled(
                format!("{symbol} {}: {}", c.name, c.detail),
                style,
            ))
        })
        .collect();

    // Same sentence the CLI prints, count included. Rendering the title from
    // `healthy` alone showed "all checks passed" above a visible `!`.
    let title = format!(" {} ", doctor::summary(report));
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}
