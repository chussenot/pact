//! The Fleet screen: who is holding what.
//!
//! Owned by pact-pyt.2, which replaces this table with an agent roster beside
//! the selected agent's work. Seeded here by the split (pact-pyt.1) with the
//! leases table this dashboard has always had, ported to the view contract —
//! so .2 starts from working code rather than a blank file.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use ratatui::crossterm::event::KeyCode;

use super::nav::View;
use super::widgets;
use super::App;
use crate::lease::{self, LeaseEntry};

#[derive(Default)]
pub struct State {
    pub leases: Vec<LeaseEntry>,
    pub table: TableState,
    /// The path the operator has selected, kept alongside the index so a
    /// refresh restores the *lease* rather than the row number. See
    /// [`widgets::reselect`].
    pub selected: Option<String>,
    /// The path armed for a force-release, awaiting a second `x`. By path and
    /// not by index, for the same reason: an index means a different lease the
    /// moment the list behind it changes.
    pub confirm_release: Option<String>,
}

pub fn refresh(app: &mut App) {
    // The one long-lived process in pact, and therefore the one that must not
    // trust a cached HEAD (pact-hxy). `git_history` memoises `head_short` per
    // repo so a batched `acquire`/`release --all` spawns one `git rev-parse`
    // instead of N — sound because HEAD cannot move inside a command that
    // exits. This session does not exit, and it can force-release from the key
    // handler, so the cache is dropped on every tick: staleness is bounded to
    // one refresh interval rather than to the lifetime of the dashboard.
    crate::git_history::forget_head(&app.repo_root);
    // peek, not list: this runs on a refresh timer, and a dashboard that
    // garbage-collects expired locks on every tick is deleting the evidence an
    // operator opened it to look at (pact-rnc.19).
    match lease::peek(&app.repo_root, true) {
        Ok(mut entries) => {
            entries.sort_by(|a, b| a.lease.path.cmp(&b.lease.path));
            app.fleet.leases = entries;
            let index = {
                let keys: Vec<&str> = app
                    .fleet
                    .leases
                    .iter()
                    .map(|e| e.lease.path.as_str())
                    .collect();
                widgets::reselect(
                    &keys,
                    app.fleet.selected.as_deref(),
                    app.fleet.table.selected(),
                )
            };
            select_index(app, index);
        }
        Err(e) => app.status = Some(format!("failed to list leases: {e:#}")),
    }
}

/// Enter opens the selected lease's path. Nothing is released, nothing is
/// written — that is what the `&App` in the signature is for.
pub fn on_enter(app: &App) -> Option<View> {
    selected_path(app).map(View::Path)
}

pub fn handle_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Char('x') => release_selected(app),
        _ => return false,
    }
    true
}

pub fn row_at(app: &App, _x: u16, y: u16) -> Option<usize> {
    widgets::row_at(app.content_area, y, 1, app.fleet.table.offset())
        .filter(|i| *i < app.fleet.leases.len())
}

pub fn select(app: &mut App, index: usize) {
    select_index(app, Some(index));
}

/// Release leads, because it is the only key here that changes anything.
pub fn help() -> &'static str {
    "x: release  j/k: move  enter: open path"
}

fn select_index(app: &mut App, index: Option<usize>) {
    app.fleet.table.select(index);
    app.fleet.selected = index
        .and_then(|i| app.fleet.leases.get(i))
        .map(|e| e.lease.path.clone());
}

fn move_selection(app: &mut App, delta: isize) {
    let index = widgets::step(app.fleet.leases.len(), app.fleet.table.selected(), delta);
    select_index(app, index);
    // Moving off the row you armed disarms it.
    app.fleet.confirm_release = None;
}

fn selected_path(app: &App) -> Option<String> {
    app.fleet
        .table
        .selected()
        .and_then(|i| app.fleet.leases.get(i))
        .map(|e| e.lease.path.clone())
}

pub(super) fn is_mine(app: &App, entry: &LeaseEntry) -> bool {
    app.agent.as_deref() == Some(entry.lease.agent.as_str())
}

/// First press on someone else's lease asks for confirmation; a second press
/// on the same lease forces the release. A press on your own releases it
/// immediately — no confirmation needed for your own claim.
///
/// This used to be on Enter. Enter is the universal look-closer key and it
/// force-released a live agent's claim from this very dashboard (pact-rnc.10),
/// so mutation now lives on a key of its own.
fn release_selected(app: &mut App) {
    let Some(index) = app.fleet.table.selected() else {
        return;
    };
    let Some(entry) = app.fleet.leases.get(index) else {
        return;
    };
    let path = entry.lease.path.clone();
    let holder = entry.lease.agent.clone();
    let mine = is_mine(app, entry);

    if app.fleet.confirm_release.as_deref() == Some(path.as_str()) {
        let agent = app.agent.clone().unwrap_or(holder);
        match lease::release(&app.repo_root, &agent, &path, true) {
            // A displaced holder means force actually took the lease off
            // someone else. Naming them is the whole value of the force path
            // here: the ui is where a human does this, and "who did I just step
            // on" is the one fact they need to go tell that agent.
            Ok(outcome) => {
                app.status = Some(match outcome.displaced() {
                    Some(displaced) => format!("force-released {path} (was held by {displaced})"),
                    None => format!("released {path}"),
                });
            }
            Err(e) => app.status = Some(format!("release failed: {e:#}")),
        }
        app.fleet.confirm_release = None;
        refresh(app);
    } else if mine {
        let agent = app.agent.clone().expect("is_mine implies agent is set");
        match lease::release(&app.repo_root, &agent, &path, false) {
            // Never Some without force: you can only displace yourself, which
            // release() reports as None.
            Ok(_) => app.status = Some(format!("released {path}")),
            Err(e) => app.status = Some(format!("release failed: {e:#}")),
        }
        refresh(app);
    } else {
        app.fleet.confirm_release = Some(path);
        app.status = Some(format!(
            "held by {holder} — press x again to force it, or esc to cancel"
        ));
    }
}

/// Whether a force-release is armed — the status line turns yellow for it.
pub(super) fn awaiting_confirmation(app: &App) -> bool {
    app.fleet.confirm_release.is_some()
}

/// Esc pops the view stack everywhere; at a root there is nothing to pop, and
/// that is where an armed release gets disarmed instead.
pub(super) fn cancel_confirm(app: &mut App) -> bool {
    if app.fleet.confirm_release.take().is_some() {
        app.status = None;
        return true;
    }
    false
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.fleet.leases.is_empty() {
        frame.render_widget(
            Paragraph::new("no active leases — press r to refresh")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    // pact-rnc.10: no raw "Remaining" countdown. A four-digit second count next
    // to a seconds-old lease read as "this long lease" and got a live agent's
    // claim force-released. Age and state say what an operator needs; both come
    // from lease.rs, so this table and `pact lease ls` cannot disagree.
    let header = Row::new(vec!["Path", "Held by", "Age", "State", "Note"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .fleet
        .leases
        .iter()
        .enumerate()
        .map(|(i, entry)| lease_row(app, i, entry))
        .collect();

    let widths = [
        Constraint::Percentage(26),
        Constraint::Percentage(14),
        Constraint::Length(8),
        // Fixed, not a percentage: "stale (reclaimable in 20s)" is the widest
        // label and the one an operator must never have truncated out from
        // under them — a half-read state is how pact-rnc.10 happened.
        Constraint::Length(26),
        Constraint::Percentage(20),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    frame.render_stateful_widget(table, area, &mut app.fleet.table.clone());
}

fn lease_row<'a>(app: &App, index: usize, entry: &'a LeaseEntry) -> Row<'a> {
    let agent_style = if is_mine(app, entry) {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    };
    let state_style = match entry.state() {
        "expired" => Style::default().fg(Color::Red),
        "stale" => Style::default().fg(Color::Yellow),
        // A suspect holder is the case this dashboard exists to catch: seven of
        // ten crucible agents stalled, and to a green `active` row they looked
        // fine. Yellow like `stale`, because both mean "look at this" — the
        // label is what says which (pact-mqw.6).
        _ if entry.suspect => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Green),
    };

    let row = Row::new(vec![
        Cell::from(entry.lease.path.as_str()),
        Cell::from(entry.lease.agent.as_str()).style(agent_style),
        Cell::from(lease::human_secs(entry.age_secs)),
        Cell::from(entry.state_label()).style(state_style),
        Cell::from(entry.lease.note.as_deref().unwrap_or("")),
    ]);

    // Selection's own reversed style is already a strong indicator; hover only
    // adds anything on rows that aren't already selected.
    if widgets::is_hovered_not_selected(app.hovered_row, app.fleet.table.selected(), index) {
        row.style(widgets::hover_style())
    } else {
        row
    }
}
