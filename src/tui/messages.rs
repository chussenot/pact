//! The message list, and the thread a message opens into.
//!
//! Owned by pact-pyt.6, which widens this from one agent's inbox to the
//! fleet's conversation. Seeded by the split (pact-pyt.1) with today's inbox
//! list and thread pane, ported to the view contract.

use std::collections::HashSet;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use ratatui::crossterm::event::KeyCode;

use super::nav::View;
use super::widgets;
use super::{App, UNREAD_INTERVAL};
use crate::msg;

pub struct State {
    pub messages: Vec<msg::Message>,
    pub list: ListState,
    /// The message id the operator selected — see [`widgets::reselect`].
    pub selected: Option<String>,
    /// Ids already marked read this session, so the 1-second refresh does not
    /// rewrite the same read cursor once per tick. A per-tick write is how a
    /// dashboard turns into a storm of pointless work.
    pub marked_read: HashSet<String>,
    /// The thread currently open, cached by its id. Loaded on the refresh tick
    /// rather than per frame — rendering runs on every keypress and mouse move,
    /// and re-reading the message store that often is exactly what the read
    /// model exists to prevent.
    pub thread: Option<(String, Vec<msg::Message>)>,
    /// Unread messages in this agent's inbox, rendered as a badge on the tab
    /// label so new traffic is visible from any screen.
    pub unread: usize,
    pub last_unread_refresh: Instant,
}

impl Default for State {
    fn default() -> Self {
        let now = Instant::now();
        State {
            messages: Vec::new(),
            list: ListState::default(),
            selected: None,
            marked_read: HashSet::new(),
            thread: None,
            unread: 0,
            // Backdated so the badge is filled in on the first refresh tick
            // instead of a whole UNREAD_INTERVAL after launch — a message
            // already waiting at startup should show up right away.
            last_unread_refresh: now.checked_sub(UNREAD_INTERVAL).unwrap_or(now),
        }
    }
}

pub fn refresh(app: &mut App) {
    if let View::Thread(id) = app.nav.current().clone() {
        refresh_thread(app, &id);
        return;
    }
    refresh_inbox(app);
}

fn refresh_inbox(app: &mut App) {
    // Set even on the early return below: with no agent there is no inbox to
    // count, and retrying that every second buys nothing.
    app.messages.last_unread_refresh = Instant::now();
    let Some(agent) = app.agent.clone() else {
        return; // rendered inline by render(); nothing to fetch
    };
    // No `app.bd` gate (pact-as5.3): the inbox is a pact file, so the message
    // panes work in a repo that has never seen the issue tracker.
    match msg::inbox(&app.repo_root, &agent, false) {
        Ok(messages) => {
            app.messages.messages = messages;
            app.messages.unread = app.messages.messages.iter().filter(|m| !m.read).count();
            let index = {
                let keys: Vec<&str> = app
                    .messages
                    .messages
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect();
                widgets::reselect(
                    &keys,
                    app.messages.selected.as_deref(),
                    app.messages.list.selected(),
                )
            };
            select_index(app, index);
            mark_selected_read(app);
        }
        Err(e) => app.status = Some(format!("failed to fetch inbox: {e:#}")),
    }
}

/// A thread is a read-only drill-in, so it must not use `msg::read_thread`:
/// that writes a read cursor, and a cursor is what a *sender* checks to decide
/// whether their message landed. Enter would have told every sender in the
/// thread that the operator's agent had received it — and, on a 1 Hz refresh,
/// told them again every second.
///
/// Filtered from `all_messages`, which fans out per recipient exactly as
/// `msg::peek_thread` does. `peek_thread` itself is `#[cfg(feature = "mcp")]`
/// and so does not exist in a `--features ui` build.
///
/// ponytail: one full parse per tick while a thread is open, which is what the
/// old `read_thread` cost too. `data::Store::thread(id)` (pact-pyt.11) is the
/// same filter over the already-parsed store — collapse this into it.
fn refresh_thread(app: &mut App, id: &str) {
    match msg::all_messages(&app.repo_root) {
        Ok(all) => {
            let thread_id = all
                .iter()
                .find(|m| m.id == id)
                .map_or_else(|| id.to_string(), |m| m.thread.clone());
            let thread = all.into_iter().filter(|m| m.thread == thread_id).collect();
            app.messages.thread = Some((id.to_string(), thread));
        }
        Err(e) => app.status = Some(format!("failed to read thread: {e:#}")),
    }
}

/// Keeps the tab-bar unread badge current from screens that don't poll the
/// inbox. The badge has to be right from anywhere — an arriving message is
/// invisible otherwise — but the inbox fetch is the expensive part, so it gets
/// its own, deliberately slower clock, checked on the 1 s wake that already
/// happens.
pub(super) fn refresh_unread_if_due(app: &mut App) {
    // A background status-line write would replace the "press x again to force
    // it" prompt mid-decision; the badge can wait.
    if super::fleet::awaiting_confirmation(app) {
        return;
    }
    if app.messages.last_unread_refresh.elapsed() >= UNREAD_INTERVAL {
        refresh_inbox(app);
    }
}

/// Enter opens the selected message's thread. `peek_thread` in `refresh` is
/// what keeps that from writing anything.
pub fn on_enter(app: &App) -> Option<View> {
    if matches!(app.nav.current(), View::Thread(_)) {
        return None; // already as deep as a message goes
    }
    app.messages
        .list
        .selected()
        .and_then(|i| app.messages.messages.get(i))
        .map(|m| View::Thread(m.id.clone()))
}

pub fn handle_key(app: &mut App, code: KeyCode) -> bool {
    if matches!(app.nav.current(), View::Thread(_)) {
        return false; // the thread pane is a read-only page; esc pops it
    }
    match code {
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        _ => return false,
    }
    true
}

pub fn row_at(app: &App, _x: u16, y: u16) -> Option<usize> {
    if matches!(app.nav.current(), View::Thread(_)) {
        return None; // nothing selectable in the thread pane
    }
    widgets::row_at(app.content_area, y, 0, app.messages.list.offset())
        .filter(|i| *i < app.messages.messages.len())
}

pub fn select(app: &mut App, index: usize) {
    select_index(app, Some(index));
    mark_selected_read(app);
}

pub fn help() -> &'static str {
    "j/k: move  enter: open thread"
}

fn select_index(app: &mut App, index: Option<usize>) {
    app.messages.list.select(index);
    app.messages.selected = index
        .and_then(|i| app.messages.messages.get(i))
        .map(|m| m.id.clone());
}

fn move_selection(app: &mut App, delta: isize) {
    let index = widgets::step(
        app.messages.messages.len(),
        app.messages.list.selected(),
        delta,
    );
    select_index(app, index);
    mark_selected_read(app);
}

/// Mark the message under the cursor as read.
///
/// The dashboard IS the human's inbox, and that was the gap: 41 of 85 messages
/// in one fleet run were addressed to `human`, who never runs `pact msg read`,
/// so `pact msg sent` reported every one of them unread forever. The protocol
/// tells agents "confirm, don't re-send: `pact msg sent` shows whether the
/// recipient has read it" — and for the single most important recipient that
/// instruction always answered no, which is how an inbox reaches sixty entries
/// nobody can triage (pact-4tj).
///
/// Selection, not display: scrolling past a line is reading it, but merely
/// opening the screen is not, and marking the whole list read on arrival would
/// destroy the unread markers that make the list worth having.
fn mark_selected_read(app: &mut App) {
    let Some(agent) = app.agent.clone() else {
        return;
    };
    let Some(m) = app
        .messages
        .list
        .selected()
        .and_then(|i| app.messages.messages.get(i))
    else {
        return;
    };
    // Already read, or already done this session: no repeated write.
    if m.read || !app.messages.marked_read.insert(m.id.clone()) {
        return;
    }
    let id = m.id.clone();
    if let Err(e) = msg::mark_read_by_id(&app.repo_root, &agent, &id) {
        // Non-fatal and non-blocking: a dashboard that cannot update read state
        // is still a dashboard. Retrying every tick would be the subprocess
        // storm this guard exists to prevent, so the id stays in the set either
        // way.
        app.status = Some(format!("could not mark {id} read: {e:#}"));
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    if let View::Thread(id) = app.nav.current().clone() {
        render_thread(frame, area, app, &id);
        return;
    }
    if let Err(e) = &app.bd {
        frame.render_widget(
            Paragraph::new(e.as_str()).style(Style::default().fg(Color::Red)),
            area,
        );
        return;
    }
    if app.agent.is_none() {
        frame.render_widget(
            Paragraph::new("no agent identity set (--agent/PACT_AGENT) — inbox unavailable")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    if app.messages.messages.is_empty() {
        frame.render_widget(
            Paragraph::new("inbox empty — press r to refresh")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .messages
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| message_list_item(app, i, m))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(ratatui::style::Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut app.messages.list.clone());
}

fn message_list_item<'a>(app: &App, index: usize, message: &'a msg::Message) -> ListItem<'a> {
    let marker = if message.read { "  " } else { "* " };
    let subject = message.subject.as_deref().unwrap_or("(no subject)");
    let mut style = if message.read {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    if widgets::is_hovered_not_selected(app.hovered_row, app.messages.list.selected(), index) {
        style = style.patch(widgets::hover_style());
    }
    ListItem::new(format!("{marker}{}  {subject}", message.id)).style(style)
}

fn render_thread(frame: &mut Frame, area: Rect, app: &App, id: &str) {
    let text = match &app.messages.thread {
        Some((cached, thread)) if cached == id => thread
            .iter()
            .map(|m| {
                format!(
                    "[{}] {}\n{}",
                    m.id,
                    m.subject.as_deref().unwrap_or("(no subject)"),
                    m.body
                )
            })
            .collect::<Vec<_>>()
            .join("\n---\n"),
        _ => "loading…".to_string(),
    };
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" thread (esc: back) "),
        ),
        area,
    );
}
