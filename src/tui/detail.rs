//! Path and Agent detail — the drill-ins Enter opens.
//!
//! Owned by pact-pyt.4 (a path's holder, custody history, messages about it,
//! its subscribers) and pact-pyt.5 (an agent's leases, events and mail). A
//! placeholder until then, but a reachable one: Enter on a lease already
//! pushes `View::Path`, so the spine is navigable and the entry point .4 and
//! .5 write into is live rather than hypothetical.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
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
    "esc: back"
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let (kind, name, bead) = match app.nav.current() {
        View::Path(path) => ("path", path.clone(), "pact-pyt.4"),
        View::Agent(agent) => ("agent", agent.clone(), "pact-pyt.5"),
        other => ("view", other.label(), "pact-pyt"),
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{kind}: {name}\n\nthe detail view lands with {bead}"
        ))
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title(" detail ")),
        area,
    );
}
