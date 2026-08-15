//! Activity: the live event feed, so an idle fleet and a finished one stop
//! looking alike.
//!
//! Owned by pact-pyt.3. A placeholder until then — the screen is new, so there
//! is nothing from the old dashboard to seed it with, but the root exists and
//! is reachable so the spine can be navigated end to end.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use ratatui::crossterm::event::KeyCode;

use super::nav::View;
use super::App;

/// Empty for now; a braced struct rather than a unit one so this bead's
/// own fields are a pure addition to this file.
#[derive(Default)]
pub struct State {}

pub fn refresh(_app: &mut App) {}

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
    "the event feed lands with pact-pyt.3"
}

pub fn render(frame: &mut Frame, area: Rect, _app: &mut App) {
    frame.render_widget(
        Paragraph::new("the event feed lands with pact-pyt.3 — `pact log` has it meanwhile")
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}
