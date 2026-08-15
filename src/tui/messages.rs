//! The fleet's conversation, and the thread a message opens into.
//!
//! This is the operator's window onto what agents are saying to each other, not
//! one agent's personal inbox (pact-pyt.6). The distinction is the whole bead:
//! the operator is usually not a fleet member, so scoping the pane to
//! `PACT_AGENT` showed them an empty screen while the fleet was talking — and
//! with an identity set it hid the one thing they most want to catch, a
//! contract change announced between two agents.
//!
//! Two separations keep it readable:
//!
//! - **Scope.** Fleet-wide by default, `m` toggles to just mine, and only when
//!   an identity exists to scope to.
//! - **Notices.** `pact watch` diffs are machine output; an agent asking a peer
//!   for a decision is not. The CLI has always split them
//!   (`--include-watch`/`--watch-only`) and [`msg::split_notices`] is that
//!   split, reused here rather than re-derived. Mixing the two is how an inbox
//!   reaches 85 entries with a BLOCKER unread inside it for 38 minutes.

use std::collections::HashSet;
use std::time::Instant;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use ratatui::crossterm::event::KeyCode;

use super::nav::View;
use super::widgets;
use super::{App, UNREAD_INTERVAL};
use crate::msg;

/// One agent-written message, rendered. Owned rather than borrowed from
/// `app.data`: the store is re-parsed under the view whenever the file changes,
/// and a row that outlives its parse is how a list starts pointing at the wrong
/// message. `to` rides along because it is what gates the read cursor below.
pub struct Row {
    pub id: String,
    /// Who it is addressed to — the gate on [`mark_selected_read`].
    pub to: String,
    pub read: bool,
    line: String,
}

pub struct State {
    /// Agent-written mail, in scope, oldest first.
    pub rows: Vec<Row>,
    /// `pact watch` notices, coalesced per path by [`msg::split_notices`] and
    /// pre-rendered — a summary, not a list. Nine diffs of one file answer one
    /// question and only the last of them answers it.
    pub notices: Vec<String>,
    pub list: ListState,
    /// The message id the operator selected — see [`widgets::reselect`].
    pub selected: Option<String>,
    /// Ids already marked read this session, so the 1-second refresh does not
    /// rewrite the same read cursor once per tick. A per-tick write is how a
    /// dashboard turns into a storm of pointless work.
    pub marked_read: HashSet<String>,
    /// The thread currently open, cached by the id it was opened from and
    /// already rendered. Built on the refresh tick rather than per frame —
    /// rendering runs on every keypress and mouse move.
    pub thread: Option<(String, String)>,
    /// Unread messages in scope, rendered as a badge on the tab label so new
    /// traffic is visible from any screen. Notices are deliberately not counted:
    /// a badge that machine output can run up is a badge nobody reads.
    pub unread: usize,
    pub last_unread_refresh: Instant,
    /// Scope: `false` is the whole fleet, which is the default and the point.
    /// Only reachable when an identity is set — there is nothing to scope to
    /// otherwise.
    pub mine: bool,
    /// Exactly where the list was last drawn, so a click and a hover hit-test
    /// against the rect rendering used rather than a second approximation of
    /// the layout (the `tab_rects` discipline, applied to a split content area).
    pub list_area: Rect,
}

impl Default for State {
    fn default() -> Self {
        let now = Instant::now();
        State {
            rows: Vec::new(),
            notices: Vec::new(),
            list: ListState::default(),
            selected: None,
            marked_read: HashSet::new(),
            thread: None,
            unread: 0,
            // Backdated so the badge is filled in on the first refresh tick
            // instead of a whole UNREAD_INTERVAL after launch — a message
            // already waiting at startup should show up right away.
            last_unread_refresh: now.checked_sub(UNREAD_INTERVAL).unwrap_or(now),
            mine: false,
            list_area: Rect::default(),
        }
    }
}

pub fn refresh(app: &mut App) {
    if let View::Thread(id) = app.nav.current().clone() {
        refresh_thread(app, &id);
        return;
    }
    refresh_list(app);
}

/// Project the message list out of the read model. No file is read here — the
/// store was parsed once for this tick before any view was asked to refresh.
fn refresh_list(app: &mut App) {
    // Set first, so the badge clock advances even on the paths that produce
    // nothing: retrying an empty store every second buys nothing.
    app.messages.last_unread_refresh = Instant::now();

    let mine = if app.messages.mine {
        app.agent.clone()
    } else {
        None
    };
    let (authored, groups) = msg::split_notices(app.data.messages());
    let rows: Vec<Row> = authored
        .into_iter()
        .filter(|m| match &mine {
            Some(me) => &m.to == me,
            None => true,
        })
        .map(|m| Row {
            id: m.id.clone(),
            to: m.to.clone(),
            read: m.read,
            line: format!(
                "{} -> {}  {}",
                m.from,
                m.to,
                m.subject.as_deref().unwrap_or("(no subject)")
            ),
        })
        .collect();
    let notices: Vec<String> = groups
        .iter()
        .map(|g| {
            let unread = if g.unread > 0 {
                format!("  {} unread", g.unread)
            } else {
                String::new()
            };
            format!(
                "{}  x{}  latest from {}{}",
                g.path, g.count, g.latest_from, unread
            )
        })
        .collect();

    app.messages.unread = rows.iter().filter(|r| !r.read).count();
    app.messages.rows = rows;
    app.messages.notices = notices;

    let index = {
        let keys: Vec<&str> = app.messages.rows.iter().map(|r| r.id.as_str()).collect();
        widgets::reselect(
            &keys,
            app.messages.selected.as_deref(),
            app.messages.list.selected(),
        )
    };
    select_index(app, index);
    mark_selected_read(app);
}

/// A thread is a read-only drill-in, so it must not use `msg::read_thread`:
/// that writes a read cursor, and a cursor is what a *sender* checks to decide
/// whether their message landed. Enter would have told every sender in the
/// thread that the operator's agent had received it — and, on a 1 Hz refresh,
/// told them again every second.
///
/// `Store::thread` is the non-marking read over the already-parsed store.
/// (`msg::peek_thread` has the same semantics but is `#[cfg(feature = "mcp")]`
/// and does not exist in a `--features ui` build.)
fn refresh_thread(app: &mut App, id: &str) {
    let text = app
        .data
        .thread(id)
        .iter()
        .map(|m| {
            format!(
                "[{}] {} -> {}  {}\n{}",
                m.id,
                m.from,
                m.to,
                m.subject.as_deref().unwrap_or("(no subject)"),
                m.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n---\n");
    app.messages.thread = Some((id.to_string(), text));
}

/// Keeps the tab-bar unread badge current from screens that don't refresh the
/// message list. The badge has to be right from anywhere — an arriving message
/// is invisible otherwise — on its own, deliberately slower clock, checked on
/// the 1 s wake that already happens.
pub(super) fn refresh_unread_if_due(app: &mut App) {
    // A background status-line write would replace the "press x again to force
    // it" prompt mid-decision; the badge can wait.
    if super::fleet::awaiting_confirmation(app) {
        return;
    }
    if app.messages.last_unread_refresh.elapsed() >= UNREAD_INTERVAL {
        refresh_list(app);
    }
}

/// Enter opens the selected message's thread. `refresh_thread` is what keeps
/// that from writing anything.
pub fn on_enter(app: &App) -> Option<View> {
    if matches!(app.nav.current(), View::Thread(_)) {
        return None; // already as deep as a message goes
    }
    app.messages
        .list
        .selected()
        .and_then(|i| app.messages.rows.get(i))
        .map(|r| View::Thread(r.id.clone()))
}

pub fn handle_key(app: &mut App, code: KeyCode) -> bool {
    if matches!(app.nav.current(), View::Thread(_)) {
        return false; // the thread pane is a read-only page; esc pops it
    }
    match code {
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Char('m') => toggle_scope(app),
        _ => return false,
    }
    true
}

pub fn row_at(app: &App, x: u16, y: u16) -> Option<usize> {
    if matches!(app.nav.current(), View::Thread(_)) {
        return None; // nothing selectable in the thread pane
    }
    // The list owns only part of the content area — the notice summary has the
    // rest — so hit-test against the rect the list was actually drawn into.
    if !widgets::rect_contains(app.messages.list_area, x, y) {
        return None;
    }
    widgets::row_at(app.messages.list_area, y, 0, app.messages.list.offset())
        .filter(|i| *i < app.messages.rows.len())
}

pub fn select(app: &mut App, index: usize) {
    select_index(app, Some(index));
    mark_selected_read(app);
}

pub fn help() -> &'static str {
    "j/k: move  m: mine/fleet  enter: open thread"
}

/// `mine` needs an identity to mean anything, and the operator often has none.
fn toggle_scope(app: &mut App) {
    if app.agent.is_none() {
        app.status =
            Some("no agent identity (--agent/PACT_AGENT) — 'mine' has nobody to scope to".into());
        return;
    }
    app.messages.mine = !app.messages.mine;
    app.status = None;
    refresh_list(app);
}

fn select_index(app: &mut App, index: Option<usize>) {
    app.messages.list.select(index);
    app.messages.selected = index
        .and_then(|i| app.messages.rows.get(i))
        .map(|r| r.id.clone());
}

fn move_selection(app: &mut App, delta: isize) {
    let index = widgets::step(app.messages.rows.len(), app.messages.list.selected(), delta);
    select_index(app, index);
    mark_selected_read(app);
}

/// Mark the message under the cursor as read — **only if it is addressed to
/// me**.
///
/// Marking on selection is deliberate: the dashboard IS the human's inbox, and
/// 41 of 85 messages in one fleet run were addressed to `human`, who never runs
/// `pact msg read`, so `pact msg sent` reported every one of them unread
/// forever (pact-4tj).
///
/// That reasoning holds for my own mail and nothing else. `mark_read_by_id`
/// records that *this* agent read it, while a sender's "did it land" check is
/// `read_by.contains(&m.to)` — so marking a message addressed to `agent-b`
/// writes my name into a cursor that tells agent-b's sender nothing, and
/// pollutes the record for everyone. Now that this pane shows the whole fleet's
/// conversation, that is most of what is on screen.
///
/// Selection, not display: scrolling past a line is reading it, but merely
/// opening the screen is not, and marking the whole list read on arrival would
/// destroy the unread markers that make the list worth having.
fn mark_selected_read(app: &mut App) {
    let Some(agent) = app.agent.clone() else {
        return;
    };
    let Some((id, to, read)) = app
        .messages
        .list
        .selected()
        .and_then(|i| app.messages.rows.get(i))
        .map(|r| (r.id.clone(), r.to.clone(), r.read))
    else {
        return;
    };
    if to != agent {
        return; // somebody else's mail: reading it here is not a delivery
    }
    // Already read, or already done this session: no repeated write.
    if read || !app.messages.marked_read.insert(id.clone()) {
        return;
    }
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

    // Notices get their own pane, sized to what there is: separating machine
    // output from correspondence is the readable half of this bead.
    let notice_height = if app.messages.notices.is_empty() {
        0
    } else {
        (app.messages.notices.len() as u16 + 2).min(7)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(notice_height)])
        .split(area);

    render_list(frame, chunks[0], app);
    if notice_height > 0 {
        render_notices(frame, chunks[1], app);
    }
}

fn render_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = match (app.messages.mine, app.agent.as_deref()) {
        (true, Some(agent)) => format!(" messages — to {agent} (m: whole fleet) "),
        _ => " messages — whole fleet (m: mine) ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Recorded so row_at hit-tests the rect the list was drawn into.
    app.messages.list_area = inner;

    if app.messages.rows.is_empty() {
        let empty = if app.messages.mine {
            "nothing addressed to you — press m for the whole fleet"
        } else {
            "no messages yet — the fleet has said nothing"
        };
        frame.render_widget(
            Paragraph::new(empty).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .messages
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| message_list_item(app, i, row))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    // The REAL state, never a clone: `render_stateful_widget` writes the scroll
    // offset it computed into whatever it is handed, and `row_at` reads that
    // offset back off `app.messages.list`. Rendering through a clone dropped it,
    // pinning the offset at 0, so every click on a scrolled list resolved a
    // screenful of rows away (pact-2ol).
    frame.render_stateful_widget(list, inner, &mut app.messages.list);
}

/// `'static` rather than borrowing `row`: the content is a `format!`, so it is
/// owned already, and a borrow here would keep `app` immutably borrowed through
/// the render call — which is exactly what forced the offset-dropping clone the
/// caller used to need.
fn message_list_item(app: &App, index: usize, row: &Row) -> ListItem<'static> {
    let marker = if row.read { "  " } else { "* " };
    let mut style = if row.read {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    if widgets::is_hovered_not_selected(app.hovered_row, app.messages.list.selected(), index) {
        style = style.patch(widgets::hover_style());
    }
    ListItem::new(format!("{marker}{}", row.line)).style(style)
}

fn render_notices(frame: &mut Frame, area: Rect, app: &App) {
    frame.render_widget(
        Paragraph::new(app.messages.notices.join("\n"))
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" watch notices (machine-written) "),
            ),
        area,
    );
}

fn render_thread(frame: &mut Frame, area: Rect, app: &App, id: &str) {
    let text = match &app.messages.thread {
        Some((cached, text)) if cached == id => text.as_str(),
        _ => "loading…",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A real store in a scratch repo. Seeding `app.messages` directly is a
    /// trap: the refresh that follows any navigation re-projects the list out
    /// of `app.data` and overwrites the fixture.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        tmp
    }

    fn send(root: &Path, from: &str, to: &str, subject: &str, notice: bool) -> String {
        let sent = msg::send(
            root,
            from,
            &[to.to_string()],
            msg::Draft {
                thread: None,
                subject: Some(subject),
                body: "body",
                about: &[],
                notice,
            },
        )
        .unwrap();
        sent[0].id.clone()
    }

    fn app_at(root: &Path, agent: Option<&str>) -> App {
        let mut app = App::new(root.to_path_buf(), agent.map(str::to_string));
        app.nav.set_root(View::Messages);
        app.refresh_current_view();
        app
    }

    fn lines(app: &App) -> Vec<&str> {
        app.messages.rows.iter().map(|r| r.line.as_str()).collect()
    }

    /// pact-2ol, in this file. `render_stateful_widget` writes the scroll offset
    /// it computed into the state it is handed; rendering through a `.clone()`
    /// dropped it, so `app.messages.list.offset()` — which `row_at` reads — was
    /// pinned at 0 and every click on a scrolled list resolved a screenful away.
    ///
    /// Invisible below one viewport of rows, which is how it survived in two
    /// call sites: it needs a list longer than the pane to reproduce at all.
    #[test]
    fn a_click_on_a_scrolled_list_lands_on_the_row_under_the_cursor() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let tmp = repo();
        for i in 0..40 {
            send(tmp.path(), "agent-a", "agent-b", &format!("subject {i:02}"), false);
        }
        let mut app = app_at(tmp.path(), None);

        // Past the bottom of the pane, so the widget must scroll to show it.
        app.messages.list.select(Some(35));
        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
        terminal.draw(|frame| super::super::draw(frame, &mut app)).unwrap();

        let offset = app.messages.list.offset();
        assert!(
            offset > 0,
            "the list must have scrolled for this test to mean anything"
        );

        // Click the first visible data row. It is the row at `offset`, not row 0.
        // x from the rect, not 0: the list is inside a bordered block, so column
        // 0 is the border and hit-tests to None.
        let y = app.messages.list_area.y;
        let x = app.messages.list_area.x;
        let hit = row_at(&app, x, y).expect("a row under the cursor");
        assert_eq!(hit, offset, "click resolved {hit}, cursor was over {offset}");
    }

    /// The headline defect: the operator is not a fleet member, and with no
    /// identity the pane was empty while the fleet was talking.
    #[test]
    fn the_fleet_conversation_is_visible_with_no_identity() {
        let tmp = repo();
        send(tmp.path(), "agent-a", "agent-b", "renamed foo()", false);
        send(tmp.path(), "agent-b", "human", "BLOCKER", false);

        let app = app_at(tmp.path(), None);
        assert_eq!(
            lines(&app),
            vec![
                "agent-a -> agent-b  renamed foo()",
                "agent-b -> human  BLOCKER",
            ]
        );
        assert_eq!(app.messages.unread, 2);
    }

    /// Watch diffs are machine output; an agent asking a peer for a decision is
    /// not. They must not share one list.
    #[test]
    fn watch_notices_are_summarised_apart_from_agent_mail() {
        let tmp = repo();
        send(tmp.path(), "agent-a", "agent-b", "renamed foo()", false);
        let notice = |holder: &str| format!("src/lease.rs{}{holder}", msg::NOTICE_SUBJECT_MARKER);
        send(tmp.path(), "agent-a", "agent-b", &notice("agent-a"), true);
        send(tmp.path(), "agent-c", "agent-b", &notice("agent-c"), true);

        let app = app_at(tmp.path(), None);
        assert_eq!(lines(&app), vec!["agent-a -> agent-b  renamed foo()"]);
        assert_eq!(app.messages.notices.len(), 1, "coalesced per path");
        assert!(app.messages.notices[0].starts_with("src/lease.rs  "));
        assert!(
            app.messages.notices[0].contains("x2"),
            "{:?}",
            app.messages.notices
        );
        assert_eq!(app.messages.unread, 1, "notices never run up the badge");
    }

    /// The trap. `mark_read_by_id` records that *this* agent read it, and a
    /// sender checks `read_by.contains(&m.to)` — so marking someone else's mail
    /// tells their sender nothing and pollutes the record.
    #[test]
    fn selecting_someone_elses_message_writes_no_read_cursor() {
        let tmp = repo();
        send(tmp.path(), "agent-a", "agent-b", "not for you", false);

        let mut app = app_at(tmp.path(), Some("operator"));
        select(&mut app, 0);

        assert!(app.messages.marked_read.is_empty());
        assert!(
            !tmp.path().join(".pact/read/operator.json").exists(),
            "no cursor for a message addressed to agent-b"
        );
    }

    #[test]
    fn selecting_my_own_message_marks_it_read_once() {
        let tmp = repo();
        let id = send(tmp.path(), "agent-a", "operator", "for you", false);

        let mut app = app_at(tmp.path(), Some("operator"));
        select(&mut app, 0);
        assert!(app.messages.marked_read.contains(&id));
        assert!(tmp.path().join(".pact/read/operator.json").exists());

        // The 1 Hz refresh must not rewrite the same cursor every tick.
        select(&mut app, 0);
        refresh(&mut app);
        assert_eq!(app.messages.marked_read.len(), 1);
    }

    #[test]
    fn the_scope_toggle_needs_an_identity_to_scope_to() {
        let tmp = repo();
        send(tmp.path(), "agent-a", "agent-b", "theirs", false);
        send(tmp.path(), "agent-a", "operator", "mine", false);

        let mut app = app_at(tmp.path(), Some("operator"));
        assert_eq!(app.messages.rows.len(), 2, "fleet-wide by default");
        handle_key(&mut app, KeyCode::Char('m'));
        assert_eq!(lines(&app), vec!["agent-a -> operator  mine"]);
        handle_key(&mut app, KeyCode::Char('m'));
        assert_eq!(app.messages.rows.len(), 2);

        let mut anon = app_at(tmp.path(), None);
        handle_key(&mut anon, KeyCode::Char('m'));
        assert!(!anon.messages.mine, "nothing to scope to");
        assert!(anon.status.is_some(), "and the operator is told why");
    }

    /// The thread pane reads through the parsed store, so it neither re-parses
    /// per tick nor marks anything read.
    #[test]
    fn opening_a_thread_marks_nothing_read() {
        let tmp = repo();
        let id = send(tmp.path(), "agent-a", "agent-b", "renamed foo()", false);

        let mut app = app_at(tmp.path(), Some("operator"));
        app.nav.push(on_enter(&app).unwrap());
        refresh(&mut app);

        let (cached, text) = app.messages.thread.as_ref().unwrap();
        assert_eq!(cached, &id);
        assert!(text.contains("agent-a -> agent-b  renamed foo()"), "{text}");
        assert!(!tmp.path().join(".pact/read/operator.json").exists());
    }
}
