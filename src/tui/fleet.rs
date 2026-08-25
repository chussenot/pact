//! The Fleet screen (pact-pyt.2, and the fleet half of pact-pyt.7): the agent
//! roster, what the selected agent is holding, and what is waiting on it.
//!
//! Two panes. LEFT, the roster — every agent this repo has ever seen, live
//! lock-holders and finished ones alike, most recent first, plus an `(all
//! leases)` row that puts the whole unfiltered lease table back. RIGHT, the
//! selected agent's work, and beneath it the contention graph: who is blocked on
//! this agent, on which path, and whether they subscribed like the protocol says
//! or are polling.
//!
//! **Why the agent and not the path.** An operator reasons about actors. "What
//! is agent X doing" used to mean reading every row of a path-sorted table, and
//! "which agents are idle" could not be asked at all — a finished agent's locks
//! are gone, so it vanished from a lease-only view. `Store::roster` merges live
//! locks with event and message history for exactly that reason.
//!
//! Nothing here reads a file. Every row comes off `App::data`, which `mod.rs`
//! refreshed once this tick; a view that re-read a store would put N parses on a
//! 1 Hz loop and, worse, could print on failure — and a stderr write smears the
//! alternate screen.

use std::collections::HashMap;

use chrono::Utc;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use ratatui::crossterm::event::KeyCode;

use super::data::{Blocked, WaitingOn, DEFAULT_GRACE_SECS};
use super::nav::View;
use super::widgets;
use super::App;
use crate::activity;
use crate::agents::AgentInfo;
use crate::lease::{self, LeaseEntry};

/// Total width of the roster column, its separating rule included.
const ROSTER_WIDTH: u16 = 34;

/// Below twice the roster's width there is not enough left for the lease table,
/// so the roster folds away and Fleet is the unfiltered table it has always
/// been. The screen degrades to something usable rather than to two unreadable
/// slivers.
const MIN_TWO_PANE_WIDTH: u16 = ROSTER_WIDTH * 2;

/// At most this many blocked agents in the waiting-on panel. The rest are still
/// one Enter away on the agent's own detail view; a panel that grows without
/// bound would eat the lease table it sits under.
const MAX_BLOCKED_ROWS: usize = 8;

/// Which pane the keyboard is driving.
///
/// Not a general focus system — there are two lists, and `h`/`l` move between
/// them. Everything else (which agent, which lease) is already selection state.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    #[default]
    Roster,
    Work,
}

#[derive(Default)]
pub struct State {
    /// Every live lock, all agents, as of the last tick — a snapshot of
    /// `Store::leases`, not a second scan of `.pact/leases/`. The work pane is a
    /// filter over this.
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

    /// Cursor in the roster pane.
    pub roster: TableState,
    /// The agent whose work the right pane is showing, by name for the same
    /// reason `selected` is by path: the roster re-sorts by recency every tick.
    /// `None` is the `(all leases)` row — every lease, nobody filtered out.
    pub agent: Option<String>,
    pub focus: Focus,
    /// Sort the roster worst-first instead of most-recent-first (pact-88z).
    ///
    /// Off by default, because the default answer to "who is here" is a fleet in
    /// the order it acted. On is the answer to "who is stuck", which is the
    /// question that makes somebody open this panel in the middle of a run — and
    /// the dead sort to the top because they are why it was opened.
    pub dead_first: bool,

    /// Unread mail per recipient, counted in one pass over the message store per
    /// tick rather than one `inbox()` scan per roster row per frame.
    unread: HashMap<String, usize>,
    /// The contention graph as of the last tick. Computed in [`refresh`] and not
    /// in [`render`] on purpose: the event loop redraws on every mouse move, and
    /// this is a pass over the whole event log. `None` until the first refresh.
    waiting: Option<WaitingOn>,
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

    // No store is read here. `data::Store` already peeked the lock directory
    // (`peek`, never `list`: a dashboard that garbage-collects expired locks
    // deletes the evidence an operator opened it to look at, pact-rnc.19).
    app.fleet.leases = app.data.leases().to_vec();
    app.fleet
        .leases
        .sort_by(|a, b| a.lease.path.cmp(&b.lease.path));

    app.fleet.unread.clear();
    for m in app.data.messages() {
        if !m.read {
            *app.fleet.unread.entry(m.to.clone()).or_default() += 1;
        }
    }

    // `waiting_on` is the whole refuse -> subscribe -> release -> notify chain,
    // and it carries the staleness bound it was computed under so the panel can
    // say on screen what "still blocked" means instead of implying a second one.
    app.fleet.waiting = Some(app.data.waiting_on(Utc::now(), DEFAULT_GRACE_SECS));

    reselect_roster(app);
    reselect_work(app);

    // The "n of m shown" indicator, over the lease table: sixty leases in one
    // path-sorted table is the case `/` exists for.
    let (shown, total) = (work_rows(app).len(), scoped_rows(app).len());
    app.filter.note(shown, total);

    // What `agents::list()` and friends would have written to stderr. Never
    // over the force-release prompt, which is the one status an operator is
    // mid-decision on.
    let diagnostics = app.data.diagnostics();
    if !diagnostics.is_empty() && app.fleet.confirm_release.is_none() {
        app.status = Some(diagnostics.join("; "));
    }
}

/// Keep the AGENT the operator selected, not the row it was on — the roster
/// re-sorts by recency, so an index means a different agent a second later. An
/// agent that has fallen out of the roster entirely falls back to `(all
/// leases)`, which is the row that can never disappear.
fn reselect_roster(app: &mut App) {
    let names: Vec<String> = roster_rows(app).iter().map(|a| a.name.clone()).collect();
    app.fleet.agent = app.fleet.agent.take().filter(|a| names.contains(a));
    let index = match (&app.fleet.agent, names.is_empty()) {
        (_, true) => None,
        (Some(agent), false) => names.iter().position(|n| n == agent),
        // The pseudo-row sits below every agent.
        (None, false) => Some(names.len()),
    };
    app.fleet.roster.select(index);
}

fn reselect_work(app: &mut App) {
    let index = {
        let rows = work_rows(app);
        let keys: Vec<&str> = rows.iter().map(|e| e.lease.path.as_str()).collect();
        widgets::reselect(
            &keys,
            app.fleet.selected.as_deref(),
            app.fleet.table.selected(),
        )
    };
    select_work(app, index);
}

/// Enter opens whatever the focused pane has selected: an agent from the
/// roster, a path from the lease table. Nothing is released, nothing is
/// written — that is what the `&App` in the signature is for.
pub fn on_enter(app: &App) -> Option<View> {
    match focus(app) {
        // `(all leases)` is a filter, not an entity: there is nothing to open.
        Focus::Roster => selected_agent(app).map(View::Agent),
        Focus::Work => selected_path(app).map(View::Path),
    }
}

pub fn handle_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Left | KeyCode::Char('h') => app.fleet.focus = Focus::Roster,
        KeyCode::Right | KeyCode::Char('l') => app.fleet.focus = Focus::Work,
        KeyCode::Char('x') => release_selected(app),
        // Toggling re-sorts under the cursor, so the selection is re-pinned to
        // the AGENT it was on rather than the row index — the same reason
        // `reselect_roster` exists at all, since the roster already re-sorts by
        // recency every tick.
        KeyCode::Char('d') => {
            app.fleet.dead_first = !app.fleet.dead_first;
            reselect_roster(app);
        }
        _ => return false,
    }
    true
}

/// Which row a point lands on — in whichever pane it was rendered in.
///
/// The two lists share one index space (roster rows first, then lease rows) so
/// that a click, a hover and a scroll all ask this one function and cannot
/// disagree about what is under the cursor. That is `tab_rects`' discipline
/// applied to a two-pane split, and it is why the panes are computed by
/// [`columns`], which rendering also calls, rather than approximated here.
pub fn row_at(app: &App, x: u16, y: u16) -> Option<usize> {
    let roster_rows = roster_len(app);
    let (roster, work) = columns(app.content_area, roster_rows);

    if let Some(roster) = roster {
        if widgets::rect_contains(roster, x, y) {
            return widgets::row_at(roster, y, 1, app.fleet.roster.offset())
                .filter(|i| *i < roster_rows);
        }
    }
    widgets::row_at(work, y, 1, app.fleet.table.offset())
        .filter(|i| *i < work_rows(app).len())
        .map(|i| i + roster_rows)
}

pub fn select(app: &mut App, index: usize) {
    let roster_rows = roster_len(app);
    if index < roster_rows {
        app.fleet.focus = Focus::Roster;
        select_roster(app, Some(index));
    } else {
        app.fleet.focus = Focus::Work;
        select_work(app, Some(index - roster_rows));
    }
}

/// Release leads, because it is the only key here that changes anything.
pub fn help() -> &'static str {
    "x: release  d: dead first  j/k: move  h/l: pane  enter: open agent/path"
}

// ----------------------------------------------------------------- selection

/// The roster as rendered: one row per agent, then `(all leases)`.
///
/// Zero when nothing has been seen in this repo — there is nothing to choose
/// between, so the pane folds away and the lease table takes the whole width
/// rather than sitting next to a column containing one dummy row.
fn roster_len(app: &App) -> usize {
    match roster_rows(app).len() {
        0 => 0,
        n => n + 1,
    }
}

/// The roster as rendered, narrowed by the filter — and the ONE list every
/// index on this pane means, so `row_at`, `select`, `on_enter` and rendering
/// cannot disagree about which agent row 3 is.
///
/// The selected agent is never filtered out from under the cursor. It scopes
/// the whole right-hand pane, so hiding it would silently re-scope the screen
/// to `(all leases)` while the operator was typing a path.
fn roster_rows(app: &App) -> Vec<&AgentInfo> {
    let mut rows: Vec<&AgentInfo> = app
        .data
        .roster()
        .iter()
        .filter(|a| {
            app.filter.matches(&[&a.name]) || app.fleet.agent.as_deref() == Some(a.name.as_str())
        })
        .collect();
    if app.fleet.dead_first {
        // `Liveness`' own ordering is the severity ordering, so this is a plain
        // sort rather than a hand-written comparator that could disagree with the
        // labels beside it. Name breaks ties so the list is stable between ticks
        // — an operator reading a row must not have it move under them because
        // two agents share a state.
        rows.sort_by(|a, b| {
            let key = |i: &AgentInfo| activity::Liveness::of(i.idle_secs, i.leases_held, false);
            key(a).cmp(&key(b)).then(a.name.cmp(&b.name))
        });
    }
    rows
}

/// Where the keyboard actually is. With no roster there is only one list, so a
/// focus of `Roster` would strand j/k on nothing.
fn focus(app: &App) -> Focus {
    if roster_len(app) == 0 {
        Focus::Work
    } else {
        app.fleet.focus
    }
}

/// The leases the right pane shows: the selected agent's, or all of them —
/// before the filter, which is what "of m" in the indicator counts.
fn scoped_rows(app: &App) -> Vec<&LeaseEntry> {
    match app.fleet.agent.as_deref() {
        Some(agent) => app
            .fleet
            .leases
            .iter()
            .filter(|e| e.lease.agent == agent)
            .collect(),
        None => app.fleet.leases.iter().collect(),
    }
}

/// The rows the lease table actually shows. Narrowed HERE, at the one place
/// this pane is projected, so the filtered list is the list `row_at` hit-tests
/// and `select` indexes into — a filter applied at render time only is a click
/// landing on a different lease than the cursor, with `x` one key away.
///
/// A lease exposes its path, its holder and its note: "who has src/lease.rs",
/// "what is docs-story holding" and "which of these are about the parser" are
/// the three questions a sixty-row table cannot answer by eye.
fn work_rows(app: &App) -> Vec<&LeaseEntry> {
    scoped_rows(app)
        .into_iter()
        .filter(|e| {
            app.filter.matches(&[
                &e.lease.path,
                &e.lease.agent,
                e.lease.note.as_deref().unwrap_or(""),
            ])
        })
        .collect()
}

/// The agent on the selected roster row, or `None` on the `(all leases)` row.
fn selected_agent(app: &App) -> Option<String> {
    app.fleet
        .roster
        .selected()
        .and_then(|i| roster_rows(app).get(i).copied())
        .map(|a| a.name.clone())
}

fn selected_path(app: &App) -> Option<String> {
    app.fleet
        .table
        .selected()
        .and_then(|i| work_rows(app).get(i).map(|e| e.lease.path.clone()))
}

fn select_work(app: &mut App, index: Option<usize>) {
    let path = index.and_then(|i| work_rows(app).get(i).map(|e| e.lease.path.clone()));
    app.fleet.table.select(index);
    app.fleet.selected = path;
}

fn select_roster(app: &mut App, index: Option<usize>) {
    let agent = index
        .and_then(|i| roster_rows(app).get(i).copied())
        .map(|a| a.name.clone());
    app.fleet.roster.select(index);
    if app.fleet.agent == agent {
        return;
    }
    // A different agent means a different list under the cursor, so the work
    // pane's selection — and anything armed against it — starts over.
    app.fleet.agent = agent;
    app.fleet.selected = None;
    app.fleet.confirm_release = None;
    let top = (!work_rows(app).is_empty()).then_some(0);
    select_work(app, top);
}

fn move_selection(app: &mut App, delta: isize) {
    match focus(app) {
        Focus::Roster => {
            let index = widgets::step(roster_len(app), app.fleet.roster.selected(), delta);
            select_roster(app, index);
        }
        Focus::Work => {
            let index = widgets::step(work_rows(app).len(), app.fleet.table.selected(), delta);
            select_work(app, index);
        }
    }
    // Moving off the row you armed disarms it.
    app.fleet.confirm_release = None;
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
    let Some((path, holder, mine)) = app.fleet.table.selected().and_then(|index| {
        let entry = work_rows(app).get(index).copied()?;
        Some((
            entry.lease.path.clone(),
            entry.lease.agent.clone(),
            is_mine(app, entry),
        ))
    }) else {
        return;
    };

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

// ------------------------------------------------------------------ geometry

/// The two columns, exactly as they are drawn: the roster's rows (its
/// separating rule excluded, because a click on the rule is not a click on an
/// agent) and the right-hand pane.
///
/// One function, called by both rendering and [`row_at`], so the two cannot
/// drift the way an independent approximation can — the reason `tab_rects`
/// exists, applied to panes.
fn columns(area: Rect, roster_rows: usize) -> (Option<Rect>, Rect) {
    if roster_rows == 0 || area.width < MIN_TWO_PANE_WIDTH {
        return (None, area);
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(ROSTER_WIDTH), Constraint::Min(0)])
        .split(area);
    let roster = Rect {
        width: cols[0].width.saturating_sub(1),
        ..cols[0]
    };
    (Some(roster), cols[1])
}

/// How the right pane splits between the lease table and the waiting-on panel:
/// a title line, one row per blocked agent, and a last line for the bound the
/// panel is judged under — never taking so much that the table it sits under
/// loses its header and its first two rows. `None` when there is genuinely no
/// room, so a very short terminal still shows the leases.
///
/// The bound is the row that goes when the terminal is squeezed, because it is
/// a caption and a blocked agent is a finding (pact-dwq).
fn stack(right: Rect, blocked: usize) -> (Rect, Option<Rect>) {
    let wanted = 2 + blocked.clamp(1, MAX_BLOCKED_ROWS) as u16;
    let height = wanted.min(right.height.saturating_sub(4));
    if height < 2 {
        return (right, None);
    }
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(height)])
        .split(right);
    (split[0], Some(split[1]))
}

/// The live blocks this panel is for: everything waiting on the selected agent,
/// or every live block when the roster is on `(all leases)`.
///
/// Live only. A refusal outlives the hold it named by the grace and no longer —
/// rendering a two-hour-old refusal as somebody currently stuck is exactly what
/// the bound exists to prevent, and the panel states the bound rather than
/// leaving the operator to guess it.
fn blocks_for(app: &App) -> Vec<&Blocked> {
    let Some(waiting) = &app.fleet.waiting else {
        return Vec::new();
    };
    match app.fleet.agent.as_deref() {
        Some(agent) => waiting
            .live()
            .filter(|b| b.holder.as_deref() == Some(agent))
            .collect(),
        None => waiting.live().collect(),
    }
}

// ------------------------------------------------------------------- drawing

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let (roster, right) = columns(area, roster_len(app));
    let (work, waiting) = stack(right, blocks_for(app).len());

    // The waiting-on panel first, and only it reads `app` immutably: both
    // tables render through their REAL `TableState`, because a state cloned per
    // frame never keeps its scroll offset — and `row_at` hit-tests against that
    // offset, so a click on a scrolled table would land on the wrong row.
    if let Some(waiting) = waiting {
        render_waiting(frame, waiting, app);
    }
    if let Some(roster) = roster {
        render_roster(frame, roster, app);
    }
    render_work(frame, work, app);
}

fn render_roster(frame: &mut Frame, area: Rect, app: &mut App) {
    // The rule lives in the column that `columns` excluded from the rows, so
    // hit-testing and drawing agree on where the agents actually are.
    let rule = Rect {
        x: area.x + area.width,
        width: 1,
        ..area
    };
    frame.render_widget(Block::default().borders(Borders::LEFT), rule);

    let focused = focus(app) == Focus::Roster;
    // No VIA column here, deliberately (pact-c3y). This panel is a narrow fixed
    // sidebar; a 16-wide column for it truncated AGENT to ten characters, which
    // turned `orchestrator` into `orchestrat` — and naming the agent is the one
    // thing the roster exists to do. The attribution chain lives on the lease
    // table, which has the width, and in full on the agent detail view, which has
    // the room to label it.
    let header = Row::new(vec!["AGENT", "HELD", "LIVE", "!"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let mut rows: Vec<Row<'static>> = roster_rows(app)
        .into_iter()
        .enumerate()
        .map(|(index, info)| {
            // A name that cannot pass `identity::validate` is one no pact
            // process could have run under — something else wrote it into the
            // store. The flag goes in FRONT of the name so it survives a
            // truncated column; rendering it like a peer is the whole defect
            // `name_valid` exists for (pact-m7j.6.3).
            let (name, name_style) = if info.name_valid {
                (info.name.clone(), Style::default())
            } else {
                (
                    format!("[INVALID] {}", info.name),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            };
            let unread = app.fleet.unread.get(&info.name).copied().unwrap_or(0);
            let row = Row::new(vec![
                Cell::from(name).style(name_style),
                Cell::from(match info.leases_held {
                    // A finished agent holds nothing and is still on the roster
                    // — that is the point of merging locks with history.
                    0 => "-".to_string(),
                    n => n.to_string(),
                }),
                liveness_cell(info),
                Cell::from(match unread {
                    0 => String::new(),
                    n => n.to_string(),
                })
                .style(Style::default().fg(Color::Cyan)),
            ]);
            hovered(app, row, index, 0, app.fleet.roster.selected(), focused)
        })
        .collect();

    // Nothing an operator has today is lost: this row is the unfiltered table.
    let index = rows.len();
    let all = Row::new(vec![
        Cell::from("(all leases)").style(Style::default().fg(Color::DarkGray)),
        Cell::from(match app.fleet.leases.len() {
            0 => "-".to_string(),
            n => n.to_string(),
        }),
    ]);
    rows.push(hovered(
        app,
        all,
        index,
        0,
        app.fleet.roster.selected(),
        focused,
    ));

    let widths = [
        Constraint::Min(10),
        Constraint::Length(4),
        // 7 is exactly "no data", the longest thing this cell holds and the one a
        // reader must not meet as "no dat".
        //
        // It REPLACES the old SEEN column rather than joining it, and that is a
        // measurement, not a preference: added alongside, at this width, it
        // turned `orchestrator` into `orchestrat` — the same crushing a 16-wide
        // column produced on this table in pact-c3y, and naming the agent is the
        // one thing this panel exists to do. Nothing is lost by the swap. SEEN
        // showed the age of the newest EVENT; this shows the age of the newest
        // pact command, which is the same signal plus the read-only half that
        // used to be invisible. The precise elapsed time is in the agent's detail
        // view, where there is room to write it out.
        Constraint::Length(7),
        Constraint::Length(2),
    ];
    // The legend, and it is not decoration (pact-88z). LIVE says an agent RAN a
    // pact command, not that it is getting anywhere: an agent busy-retrying a
    // lease it will never get renders ACTIVE, in green, and is exactly the
    // pathology `--check retry-storm` exists to catch. A green cell is precisely
    // where that limit gets forgotten, so the caption sits under the column that
    // invites the mistake.
    //
    // Only when the roster is tall enough to spare the room — an operator's
    // answer must never be pushed off-screen by the footnote to it.
    let area = if area.height > 5 {
        frame.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "LIVE = ran a pact command, not that it is progressing. \
                 STALE/DEAD still hold. \"no data\" = no record on this machine.",
                Style::default().fg(Color::DarkGray),
            ))])
            .wrap(Wrap { trim: true }),
            Rect {
                y: area.y + area.height - 2,
                height: 2,
                ..area
            },
        );
        Rect {
            height: area.height - 2,
            ..area
        }
    } else {
        area
    };

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(selection_style(focused))
        .highlight_symbol(if focused { "> " } else { "  " });

    frame.render_stateful_widget(table, area, &mut app.fleet.roster);
}

fn render_work(frame: &mut Frame, area: Rect, app: &mut App) {
    if work_rows(app).is_empty() {
        let message = match app.fleet.agent.as_deref() {
            Some(agent) => format!("{agent} holds nothing right now"),
            None => "no active leases — press r to refresh".to_string(),
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    // pact-rnc.10: no raw "Remaining" countdown. A four-digit second count next
    // to a seconds-old lease read as "this long lease" and got a live agent's
    // claim force-released. Age and state say what an operator needs; both come
    // from lease.rs, so this table and `pact lease ls` cannot disagree.
    // No attribution column here, and that is a decision rather than an omission
    // (pact-c3y). It was tried, at three widths, and every one of them cost
    // something that outranks it: a fixed 24 (enough for `claude-code
    // ~sonnet-4-6`) crushed Path to `Pa` and Held by to `He` at 120 columns; 12%
    // was 11 characters, which cut `~sonnet-4-6` itself into a shorter, plausible,
    // WRONG model name; 14 fixed broke three other layout tests. This table's
    // columns are already load-bearing — pact-rnc.10 is the incident where a
    // half-read value on this screen got a live agent's lease force-released.
    //
    // The chain is in the detail view instead (`Enter` on a path or an agent),
    // which has the room to write `model sonnet-4-6 (declared)` in words rather
    // than compressing it to a marker nobody can read at a glance.
    let header = Row::new(vec!["Path", "Held by", "Age", "State", "Note"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let focused = focus(app) == Focus::Work;
    // The lease table's rows start past the whole roster in the shared index
    // space `row_at` returns, so hover has to be read in the same coordinates.
    let offset = roster_len(app);
    let rows: Vec<Row<'static>> = work_rows(app)
        .iter()
        .enumerate()
        .map(|(i, entry)| lease_row(app, i, offset, entry, focused))
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
        .row_highlight_style(selection_style(focused))
        .highlight_symbol(if focused { "> " } else { "  " });

    frame.render_stateful_widget(table, area, &mut app.fleet.table);
}

/// The contention graph — the highest-value thing the event log carries and no
/// surface showed. `refused` names the holder and the holder's own remaining
/// lease; the watch registry says whether the blocked agent then subscribed;
/// `pact audit --check retry-storm` says whether it polled instead.
fn render_waiting(frame: &mut Frame, area: Rect, app: &App) {
    let blocked = blocks_for(app);
    let who = match app.fleet.agent.as_deref() {
        Some(agent) => format!("waiting on {agent}"),
        None => "waiting on the fleet".to_string(),
    };
    // The bound, on screen. `WaitingOn` carries the grace it was computed under
    // precisely so the panel states what "live" means instead of implying a
    // second, invisible one.
    let grace = app
        .fleet
        .waiting
        .as_ref()
        .map_or(DEFAULT_GRACE_SECS, |w| w.grace_secs);
    // The title carries only the short half. Ratatui truncates a block title
    // SILENTLY, and this panel lives in the right-hand pane, so at real widths
    // the sentence explaining the bound was the part that got cut — leaving
    // "live = holder's remai" and implying exactly the invisible bound the
    // grace is threaded through here to avoid (pact-dwq). Every test asserted
    // on the rows, and a title is not a row.
    let block = Block::default()
        .borders(Borders::TOP)
        .title(format!(" {who} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Stated inside the panel instead, where a Paragraph wraps rather than
    // clips, and dim because it is a caption rather than a finding.
    let bound = Line::from(Span::styled(
        format!(
            "live = the holder's remaining time + {} grace",
            lease::human_secs(grace)
        ),
        Style::default().fg(Color::DarkGray),
    ));

    let mut lines: Vec<Line> = if blocked.is_empty() {
        vec![Line::from(Span::styled(
            "nobody is blocked",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        blocked.iter().map(|b| blocked_line(app, b)).collect()
    };
    lines.push(bound);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// One edge, read left to right: who is stuck, on what, for how long, behind
/// whom — and then the fact that decides whether this is contention or a
/// protocol failure.
fn blocked_line<'a>(app: &App, b: &'a Blocked) -> Line<'a> {
    let holder = b.holder.as_deref().unwrap_or("someone");
    // A holder that has gone quiet is named as quiet, not merely coloured:
    // seven of ten crucible agents stalled while every row read green
    // (pact-mqw.6), and "orchestrator 39m53s left" is exactly the reassuring
    // sentence that hid it.
    let quiet = app
        .fleet
        .leases
        .iter()
        .find(|e| e.lease.path == b.path && e.lease.agent == holder)
        .filter(|e| e.suspect)
        .map(|e| e.state_label());

    let (status, status_style) = if b.retry_storm {
        // The storm judgement is `pact audit --check retry-storm`'s own, not a
        // second implementation of "is this a poll loop" that could disagree.
        (
            if b.subscribed {
                "RETRYING (already subscribed)"
            } else {
                "RETRYING"
            },
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if b.subscribed {
        ("subscribed", Style::default().fg(Color::Green))
    } else {
        ("not subscribed", Style::default().fg(Color::Yellow))
    };

    let mut spans = vec![
        Span::styled(
            b.agent.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" wants "),
        Span::styled(b.path.clone(), Style::default().fg(Color::Cyan)),
        Span::raw(format!(" · {} waiting · held by ", human(b.waited_secs))),
    ];
    match quiet {
        Some(label) => spans.push(Span::styled(
            format!("{holder} ({label})"),
            Style::default().fg(Color::Yellow),
        )),
        None => spans.push(Span::raw(match b.holder_remaining_secs {
            Some(left) => format!("{holder} ({} left)", human(left)),
            None => holder.to_string(),
        })),
    }
    spans.push(Span::raw(" · "));
    spans.push(Span::styled(status, status_style));
    if b.refusals > 1 {
        spans.push(Span::raw(format!(" · {} refusals", b.refusals)));
    }
    Line::from(spans)
}

fn lease_row(
    app: &App,
    index: usize,
    offset: usize,
    entry: &LeaseEntry,
    focused: bool,
) -> Row<'static> {
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

    // Owned cells, not borrows of the lease: the table is handed to
    // `render_stateful_widget` alongside `&mut app.fleet.table`, and a row still
    // borrowing `app.fleet.leases` would keep that mutable borrow out of reach.
    let row = Row::new(vec![
        Cell::from(entry.lease.path.clone()),
        Cell::from(entry.lease.agent.clone()).style(agent_style),
        Cell::from(lease::human_secs(entry.age_secs)),
        Cell::from(entry.state_label()).style(state_style),
        Cell::from(entry.lease.note.clone().unwrap_or_default()),
    ]);

    hovered(app, row, index, offset, app.fleet.table.selected(), focused)
}

/// The LIVE cell: is anybody home (pact-88z)?
///
/// Four states and a non-state. `ACTIVE` means this agent ran SOME pact command
/// inside the freshness window — including the read-only ones that write no
/// event, which is the whole reason this exists. `STALE` is the one worth acting
/// on: quiet past the window and still holding, so work is parked behind a lease
/// nobody is behind. `IDLE` is quiet and holding nothing, which blocks nobody.
/// `DEAD` is past TTL on everything it holds.
///
/// `no data` is not a verdict and is styled as absence rather than as alarm. A
/// repository whose fleet ran on a pact older than this has no records at all,
/// and rendering that as DEAD would report every historical agent as a corpse.
///
/// **This says the agent used pact, not that it is making progress.** An agent
/// busy-retrying a lease it will never get is maximally ACTIVE here and is
/// exactly what `pact audit --check retry-storm` exists to catch. The legend
/// under the roster says so, because a colour that reads as "fine" is the one
/// place that limit will be forgotten.
fn liveness_cell(info: &AgentInfo) -> Cell<'static> {
    // `all_expired` is false here: the roster counts leases held, and whether
    // each is past TTL is the lease table's question, asked with the lease in
    // hand. Overclaiming DEAD from a count alone would be the worse error.
    let state = activity::Liveness::of(info.idle_secs, info.leases_held, false);
    let style = match state {
        // STALE and DEAD are the reasons an operator opened this panel, so they
        // are the two that must be findable without reading.
        activity::Liveness::Dead => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        activity::Liveness::Stale => Style::default().fg(Color::Yellow),
        activity::Liveness::Active => Style::default().fg(Color::Green),
        activity::Liveness::Idle => Style::default(),
        activity::Liveness::NoData => Style::default().fg(Color::DarkGray),
    };
    Cell::from(state.label()).style(style)
}

/// Selection's own reversed style is already a strong indicator; hover only
/// adds anything on rows that aren't already selected, and only in the pane the
/// keyboard is actually driving.
///
/// `offset` is where this pane starts in the shared index space [`row_at`]
/// returns — 0 for the roster, past the whole roster for the lease table.
/// Without it the two panes read the same `hovered_row` as their own row number
/// and both light up, which is the same class of drift `tab_rects` exists to
/// prevent.
fn hovered(
    app: &App,
    row: Row<'static>,
    index: usize,
    offset: usize,
    selected: Option<usize>,
    focused: bool,
) -> Row<'static> {
    let hovered = app.hovered_row.and_then(|row| row.checked_sub(offset));
    if focused && widgets::is_hovered_not_selected(hovered, selected, index) {
        row.style(widgets::hover_style())
    } else {
        row
    }
}

/// The unfocused pane keeps its cursor visible but dimmed, so an operator can
/// see what `x` would act on without the two panes both claiming to be current.
fn selection_style(focused: bool) -> Style {
    if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().add_modifier(Modifier::DIM | Modifier::REVERSED)
    }
}

/// `lease::human_secs` clamps a negative to `0s`, which is right for a lease
/// countdown and wrong for a stamp a few seconds into the future because of
/// clock skew — but "0s ago" is the honest reading of that, so it is reused
/// rather than reimplemented.
fn human(secs: i64) -> String {
    lease::human_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;
    use std::path::Path;

    /// Fixture stores, written as the raw JSONL pact appends — so the parse
    /// path is under test too, and every timestamp is chosen rather than "now".
    fn write(root: &Path, name: &str, lines: &[serde_json::Value]) {
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(root.join(".pact").join(name), body).unwrap();
    }

    fn event(at: &str, agent: &str, kind: &str, path: &str) -> serde_json::Value {
        serde_json::json!({ "at": at, "agent": agent, "kind": kind, "path": path, "detail": null })
    }

    fn refusal(at: &str, agent: &str, path: &str, holder: &str, left: i64) -> serde_json::Value {
        serde_json::json!({
            "at": at, "agent": agent, "kind": "refused", "path": path, "detail": null,
            "holder": holder, "holder_remaining_secs": left,
        })
    }

    /// The chain this repo's own log recorded: docs-story is refused
    /// docs/tui.md while orchestrator holds it, and subscribes 17s later.
    /// `poller` is refused the same path and never subscribes at all.
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".pact")).unwrap();
        write(
            tmp.path(),
            "events.jsonl",
            &[
                event(
                    "2026-08-14T14:30:00+00:00",
                    "orchestrator",
                    "acquired",
                    "docs/tui.md",
                ),
                refusal(
                    "2026-08-14T14:37:25+00:00",
                    "docs-story",
                    "docs/tui.md",
                    "orchestrator",
                    2393,
                ),
                event(
                    "2026-08-14T14:37:42+00:00",
                    "docs-story",
                    "watched",
                    "docs/tui.md",
                ),
                refusal(
                    "2026-08-14T14:38:00+00:00",
                    "poller",
                    "docs/tui.md",
                    "orchestrator",
                    2350,
                ),
                // A name no pact process could have run under: something other
                // than pact wrote it into the store.
                event(
                    "2026-08-14T14:20:00+00:00",
                    "Impostor",
                    "acquired",
                    "src/x.rs",
                ),
            ],
        );
        write(
            tmp.path(),
            "watches.jsonl",
            &[serde_json::json!({
                "at": "2026-08-14T14:37:42+00:00", "agent": "docs-story",
                "kind": "watch", "path": "docs/tui.md",
            })],
        );
        tmp
    }

    fn entry(agent: &str, path: &str, age: i64) -> LeaseEntry {
        LeaseEntry {
            lease: lease::LeaseInfo {
                agent: agent.to_string(),
                path: path.to_string(),
                acquired_at: "2026-08-14T14:30:00+00:00".to_string(),
                ttl_secs: 2700,
                note: None,
                branch: None,
                worktree: None,
                invoked_from: None,
                content_hash: None,
                harness: None,
                model: None,
                extra: Default::default(),
            },
            age_secs: age,
            remaining_secs: 2700 - age,
            expired: false,
            holder_silent_secs: None,
            suspect: false,
        }
    }

    /// A real `App` over a real fixture repo — the roster, the watch registry
    /// and the contention graph all come from the store, as they do in
    /// production. The leases are seeded afterwards because a lock file is
    /// state, not history, and nothing in the fixture creates one.
    fn app(tmp: &Path, agent: Option<&str>, leases: Vec<LeaseEntry>) -> App {
        let mut app = App::new(tmp.to_path_buf(), agent.map(str::to_string));
        app.content_area = Rect::new(0, 3, 100, 16);
        app.fleet.leases = leases;
        reselect_work(&mut app);
        app
    }

    fn drawn(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, app))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// pact-88z: `d` puts the worst first, because the worst are why the panel
    /// was opened.
    ///
    /// Default order is recency, which answers "who is here". This answers "who
    /// is stuck", and the two are different questions — an operator mid-run wants
    /// the second and should not have to read the whole roster to get it.
    ///
    /// The selection is asserted to survive the re-sort. That is not incidental:
    /// the roster already re-sorts by recency every tick, which is why
    /// `reselect_roster` keys on the AGENT and not the row, and a toggle that
    /// moved the cursor to a different agent would be a worse bug than no toggle.
    #[test]
    fn d_sorts_the_worst_first_and_keeps_the_operator_on_their_agent() {
        let tmp = fixture();
        let dir = crate::repo::pact_dir_path(tmp.path()).join("activity");
        std::fs::create_dir_all(&dir).unwrap();
        let ago = |s: i64| (Utc::now() - chrono::Duration::seconds(s)).to_rfc3339();
        // poller is ACTIVE, docs-story is IDLE. Recency puts poller first.
        std::fs::write(dir.join("poller"), ago(5)).unwrap();
        std::fs::write(
            dir.join("docs-story"),
            ago(crate::activity::FRESH_SECS + 60),
        )
        .unwrap();

        let mut app = app(tmp.path(), Some("operator"), vec![]);
        app.fleet.agent = Some("poller".to_string());
        reselect_roster(&mut app);
        let before = app.fleet.roster.selected();

        let order = |app: &App| -> Vec<String> {
            roster_rows(app).iter().map(|a| a.name.clone()).collect()
        };
        let recency = order(&app);

        assert!(handle_key(&mut app, KeyCode::Char('d')), "d is handled");
        let worst_first = order(&app);

        assert_ne!(recency, worst_first, "the toggle must actually reorder");
        let idle_at = worst_first.iter().position(|n| n == "docs-story").unwrap();
        let active_at = worst_first.iter().position(|n| n == "poller").unwrap();
        assert!(
            idle_at < active_at,
            "IDLE outranks ACTIVE when sorting by what is wrong: {worst_first:?}"
        );

        // And the cursor is still on the operator's agent, not on whatever row
        // that index now holds.
        assert_eq!(app.fleet.agent.as_deref(), Some("poller"));
        let after = app.fleet.roster.selected();
        assert!(
            before != after || recency == worst_first,
            "a re-sort that moved the agent must have moved the cursor with it"
        );

        // Toggling back restores the default order.
        assert!(handle_key(&mut app, KeyCode::Char('d')));
        assert_eq!(order(&app), recency);
    }

    /// pact-88z: the four states an operator scans for, plus the non-state.
    ///
    /// Rendered from fixture activity records, because the classification is only
    /// worth anything if it survives the panel — a helper returning the right
    /// word while the column that shows it is truncated, dropped or reordered is
    /// the failure this repository has already had twice on this exact table
    /// (pact-rnc.10, pact-c3y).
    ///
    /// `no data` is asserted as loudly as the rest. A repository whose fleet ran
    /// on a pact older than this has no records at all, and rendering that as
    /// DEAD would report every historical agent as a corpse.
    #[test]
    fn the_roster_shows_each_liveness_state_and_names_the_absent_one() {
        let tmp = fixture();
        let state = crate::repo::pact_dir_path(tmp.path());
        let dir = state.join("activity");
        std::fs::create_dir_all(&dir).unwrap();
        let ago = |secs: i64| (Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339();

        // orchestrator holds a REAL lock — written through `lease::acquire`, so the
        // store's own `peek` finds it — and is quiet past the window: STALE. The
        // lock has to be real because `leases_held` is rebuilt from the store, not
        // from the fixture handed to the view, and STALE is precisely the state
        // that distinguishes "quiet" from "quiet AND holding".
        crate::lease::acquire(tmp.path(), "orchestrator", "docs/tui.md", 2700, false, None)
            .unwrap();
        std::fs::write(
            dir.join("orchestrator"),
            ago(crate::activity::FRESH_SECS + 60),
        )
        .unwrap();
        // docs-story holds nothing and is quiet -> IDLE.
        std::fs::write(
            dir.join("docs-story"),
            ago(crate::activity::FRESH_SECS + 60),
        )
        .unwrap();
        // poller ran something a moment ago -> ACTIVE.
        std::fs::write(dir.join("poller"), ago(5)).unwrap();
        // and the fixture's fourth agent gets no record at all.

        let held = entry("orchestrator", "docs/tui.md", 90);
        let mut app = app(tmp.path(), Some("operator"), vec![held]);
        let screen = drawn(&mut app, 160, 24);

        for want in ["STALE", "IDLE", "ACTIVE", "no data"] {
            assert!(
                screen.contains(want),
                "the roster must show {want}: {screen}"
            );
        }
        // The honesty rail, on screen rather than only in a doc comment: a green
        // cell is where "alive" gets read as "progressing", and an agent spinning
        // on a refused lease is maximally ACTIVE here.
        // Asserted in pieces, because the caption wraps in a narrow pane and a
        // whole-sentence match would pass or fail on the pane width rather than
        // on the words being there.
        for want in ["ran a pact command", "progressing"] {
            assert!(
                screen.contains(want),
                "the legend must say what LIVE does not mean ({want}): {screen}"
            );
        }
    }

    /// An agent whose every hold is past TTL is DEAD, and that outranks how
    /// recently it ran something — a lock anyone may reclaim is the louder fact.
    #[test]
    fn every_hold_past_ttl_reads_as_dead() {
        use crate::activity::Liveness;
        assert_eq!(Liveness::of(Some(5), 1, true), Liveness::Dead);
        assert_eq!(Liveness::of(Some(5), 1, false), Liveness::Active);
        assert_eq!(
            Liveness::of(Some(crate::activity::FRESH_SECS), 1, false),
            Liveness::Stale
        );
        assert_eq!(
            Liveness::of(Some(crate::activity::FRESH_SECS), 0, false),
            Liveness::Idle
        );
        assert_eq!(Liveness::of(None, 3, true), Liveness::NoData);
    }

    /// The landing screen shows every agent seen in this repo — live holders
    /// and finished ones alike — which is the question a lease-only view could
    /// not answer at all.
    #[test]
    fn the_roster_lists_finished_agents_not_just_lock_holders() {
        let tmp = fixture();
        let mut app = app(tmp.path(), Some("operator"), vec![]);

        let rendered = drawn(&mut app, 120, 22);
        for name in ["orchestrator", "docs-story", "poller"] {
            assert!(rendered.contains(name), "{name} missing from: {rendered}");
        }
        assert!(rendered.contains("(all leases)"), "{rendered}");
    }

    /// A name that cannot pass `identity::validate` is not rendered as a peer.
    #[test]
    fn an_unvalidatable_name_is_flagged_in_front_so_truncation_cannot_hide_it() {
        let tmp = fixture();
        let mut app = app(tmp.path(), Some("operator"), vec![]);
        assert!(drawn(&mut app, 120, 22).contains("[INVALID]"));
    }

    /// Selecting an agent filters the work pane; `(all leases)` puts today's
    /// unfiltered table back, so nothing an operator has is lost.
    #[test]
    fn an_agent_filters_the_lease_table_and_all_leases_restores_it() {
        let tmp = fixture();
        let leases = vec![
            entry("orchestrator", "docs/tui.md", 600),
            entry("quern", "src/audit.rs", 120),
        ];
        let mut app = app(tmp.path(), Some("operator"), leases);

        let index = app
            .data
            .roster()
            .iter()
            .position(|a| a.name == "orchestrator")
            .expect("orchestrator is on the roster");
        select(&mut app, index);
        assert_eq!(app.fleet.agent.as_deref(), Some("orchestrator"));
        assert_eq!(work_rows(&app).len(), 1);
        assert_eq!(app.fleet.selected.as_deref(), Some("docs/tui.md"));

        // The pseudo-row sits below every agent and is never filtered out.
        let all = roster_len(&app) - 1;
        select(&mut app, all);
        assert_eq!(app.fleet.agent, None);
        assert_eq!(work_rows(&app).len(), 2);
    }

    /// pact-pyt.7 on the fleet screen: who is blocked, on whom, and whether
    /// they subscribed or are polling — without reading the raw feed.
    #[test]
    fn the_waiting_on_panel_names_the_blocked_agent_its_holder_and_its_conduct() {
        let tmp = fixture();
        let mut app = app(
            tmp.path(),
            Some("operator"),
            vec![entry("orchestrator", "docs/tui.md", 600)],
        );
        // The panel is live-only, so the fixture has to be read as of a moment
        // inside the hold it named.
        app.fleet.waiting = Some(
            app.data.waiting_on(
                chrono::DateTime::parse_from_rfc3339("2026-08-14T14:40:00+00:00")
                    .unwrap()
                    .with_timezone(&Utc),
                DEFAULT_GRACE_SECS,
            ),
        );

        let rendered = drawn(&mut app, 160, 22);
        assert!(
            rendered.contains("docs-story wants docs/tui.md"),
            "{rendered}"
        );
        assert!(rendered.contains("held by orchestrator"), "{rendered}");
        assert!(rendered.contains("subscribed"), "{rendered}");
        // An agent that was refused and never subscribed reads differently from
        // one that did — that distinction is the point of the panel.
        assert!(rendered.contains("not subscribed"), "{rendered}");
        // And the window "live" was judged against is stated, not implied.
        //
        // This assertion existed before pact-dwq and passed throughout it: at
        // 160 columns the sentence fit in the block title it used to live in.
        // The truncation only appears at widths an operator actually uses, so
        // the gap was never the assertion — it was the terminal size. See
        // `the_grace_bound_survives_at_a_real_terminal_width`.
        assert!(
            rendered.contains("live = the holder's remaining time"),
            "{rendered}"
        );
        assert!(rendered.contains("5m0s grace"), "{rendered}");
    }

    /// A refusal older than the hold it named plus the grace is history, not
    /// somebody currently stuck.
    #[test]
    fn a_stale_refusal_is_not_presented_as_a_live_block() {
        let tmp = fixture();
        let mut app = app(tmp.path(), Some("operator"), vec![]);
        app.fleet.waiting = Some(
            app.data.waiting_on(
                chrono::DateTime::parse_from_rfc3339("2026-08-14T18:00:00+00:00")
                    .unwrap()
                    .with_timezone(&Utc),
                DEFAULT_GRACE_SECS,
            ),
        );
        assert!(blocks_for(&app).is_empty());
        assert!(drawn(&mut app, 160, 22).contains("nobody is blocked"));
    }

    /// Enter is the universal look-closer key on both panes, and it mutates
    /// nothing anywhere — `on_enter` takes `&App` so that is a compile error.
    #[test]
    fn enter_opens_an_agent_from_the_roster_and_a_path_from_the_table() {
        let tmp = fixture();
        let mut app = app(
            tmp.path(),
            Some("operator"),
            vec![entry("orchestrator", "docs/tui.md", 600)],
        );

        let index = app
            .data
            .roster()
            .iter()
            .position(|a| a.name == "docs-story")
            .unwrap();
        select(&mut app, index);
        assert_eq!(on_enter(&app), Some(View::Agent("docs-story".to_string())));

        // `(all leases)` is a filter, not an entity: there is nothing to open.
        let rows = roster_len(&app);
        select(&mut app, rows - 1);
        assert_eq!(on_enter(&app), None);

        select(&mut app, rows); // the first lease row
        assert_eq!(on_enter(&app), Some(View::Path("docs/tui.md".to_string())));
        assert_eq!(app.fleet.confirm_release, None);
    }

    /// One index space, two panes: a click is resolved by the same function
    /// that laid them out, so it can never land on a row nobody drew there.
    #[test]
    fn a_click_resolves_to_the_pane_it_was_rendered_in() {
        let tmp = fixture();
        let mut app = app(
            tmp.path(),
            Some("operator"),
            vec![
                entry("orchestrator", "docs/tui.md", 600),
                entry("quern", "src/audit.rs", 120),
            ],
        );
        drawn(&mut app, 120, 22);

        let (roster, work) = columns(app.content_area, roster_len(&app));
        let roster = roster.expect("wide enough for two panes");

        // First roster row, below its column header.
        assert_eq!(row_at(&app, roster.x, roster.y + 1), Some(0));
        // First lease row, in the other pane, offset past the whole roster.
        assert_eq!(row_at(&app, work.x + 2, work.y + 1), Some(roster_len(&app)));
        // The column headers themselves select nothing.
        assert_eq!(row_at(&app, roster.x, roster.y), None);
        // Neither does empty space below the last lease.
        assert_eq!(row_at(&app, work.x + 2, work.y + 9), None);
    }

    /// The tables render through their real `TableState`, so a scrolled table's
    /// offset survives the frame — and `row_at`, which hit-tests against that
    /// offset, keeps agreeing with what is on screen. A state cloned per frame
    /// pinned the offset at 0 and put every click on a scrolled table one
    /// screenful of rows away from the one under the cursor.
    /// pact-dwq. The panel threads `WaitingOn::grace_secs` all the way here so
    /// it can state what "live" means rather than imply an invisible bound —
    /// and then put that sentence in a Block title, which ratatui truncates
    /// SILENTLY. At the real right-pane width the operator saw
    /// "live = holder's remai".
    ///
    /// The coverage was not missing — `the_waiting_on_panel_names_...` already
    /// asserted the bound appears in the rendered buffer, and passed all the
    /// way through, because it renders at 160 columns where the title fitted.
    /// The gap was the WIDTH. This renders at 100, and also fails on a clipped
    /// word so a future title that overflows is caught rather than shortened.
    #[test]
    fn the_grace_bound_survives_at_a_real_terminal_width() {
        let tmp = fixture();
        let mut app = app(
            tmp.path(),
            Some("operator"),
            vec![entry("orchestrator", "docs/tui.md", 600)],
        );
        // 100 columns: the width the truncation was observed at, and narrower
        // than the 120 the other render tests use.
        let rendered = drawn(&mut app, 100, 24);
        assert!(
            rendered.contains("grace"),
            "the bound must be legible, not clipped: {rendered}"
        );
        assert!(
            !rendered.contains("remai "),
            "a clipped word means the title is truncating again: {rendered}"
        );
    }

    #[test]
    fn a_scrolled_table_still_maps_a_click_to_the_row_under_the_cursor() {
        let tmp = fixture();
        let leases: Vec<LeaseEntry> = (0..20)
            .map(|i| entry("quern", &format!("src/f{i:02}.rs"), 60))
            .collect();
        let mut app = app(tmp.path(), Some("operator"), leases);

        app.fleet.focus = Focus::Work;
        for _ in 0..19 {
            move_selection(&mut app, 1);
        }
        drawn(&mut app, 120, 14);

        let offset = app.fleet.table.offset();
        assert!(offset > 0, "20 leases in 7 rows should have scrolled");
        let (_, work) = columns(app.content_area, roster_len(&app));
        assert_eq!(
            row_at(&app, work.x + 2, work.y + 1),
            Some(roster_len(&app) + offset),
            "the top visible row is the scrolled-to one, not row 0"
        );
    }

    /// The operator is not a fleet member: with no PACT_AGENT there is no
    /// "mine" to colour and no inbox to count, and the screen still works.
    #[test]
    fn the_screen_works_with_no_pact_agent_set() {
        let tmp = fixture();
        let mut app = app(
            tmp.path(),
            None,
            vec![entry("orchestrator", "docs/tui.md", 600)],
        );
        let rendered = drawn(&mut app, 120, 22);
        assert!(rendered.contains("orchestrator"), "{rendered}");
        assert!(rendered.contains("docs/tui.md"), "{rendered}");
    }

    /// The roster re-sorts by recency on every tick, so the cursor has to hold
    /// onto the AGENT rather than the row it happened to be on.
    #[test]
    fn a_reordered_roster_does_not_move_the_operators_agent() {
        let tmp = fixture();
        let mut app = app(tmp.path(), Some("operator"), vec![]);

        let index = app
            .data
            .roster()
            .iter()
            .position(|a| a.name == "orchestrator")
            .unwrap();
        select(&mut app, index);

        // A later event by another agent pushes orchestrator down the roster.
        write(
            tmp.path(),
            "events.jsonl",
            &[
                event(
                    "2026-08-14T14:30:00+00:00",
                    "orchestrator",
                    "acquired",
                    "docs/tui.md",
                ),
                event(
                    "2026-08-14T15:00:00+00:00",
                    "docs-story",
                    "acquired",
                    "src/z.rs",
                ),
            ],
        );
        app.data.refresh(tmp.path());
        refresh(&mut app);

        assert_eq!(app.fleet.agent.as_deref(), Some("orchestrator"));
        let moved = app.fleet.roster.selected().unwrap();
        assert_eq!(app.data.roster()[moved].name, "orchestrator");
    }

    /// With nothing seen in this repo there is nothing to choose between, so
    /// the roster folds away rather than showing one dummy row beside a
    /// squeezed table — and Fleet is the unfiltered table it has always been.
    #[test]
    fn an_empty_roster_gives_the_lease_table_the_whole_width() {
        let content = Rect::new(0, 3, 100, 16);
        assert_eq!(columns(content, 0), (None, content));
        // And so does a terminal too narrow to split.
        let narrow = Rect::new(0, 3, 60, 16);
        assert_eq!(columns(narrow, 4), (None, narrow));
    }

    /// Type a query straight into the shared filter — the path mod.rs's key
    /// handler takes — without going through `refresh`, which would re-read the
    /// (lock-file-free) fixture and drop the seeded leases.
    fn query(app: &mut App, text: &str) {
        app.filter.open();
        for c in text.chars() {
            app.filter.key(KeyCode::Char(c));
        }
    }

    /// pact-pyt.9: sixty leases in one path-sorted table is the case this
    /// exists for. A lease is matched on any of the three things an operator
    /// looks for it by.
    #[test]
    fn the_filter_narrows_the_lease_table_by_path_holder_or_note() {
        let tmp = fixture();
        let mut noted = entry("quern", "src/audit.rs", 120);
        noted.lease.note = Some("rewriting the summary".to_string());
        let mut app = app(
            tmp.path(),
            Some("operator"),
            vec![entry("orchestrator", "docs/tui.md", 600), noted],
        );
        assert_eq!(work_rows(&app).len(), 2);

        query(&mut app, "docs/");
        assert_eq!(work_rows(&app).len(), 1, "by path");
        app.filter.clear();

        query(&mut app, "quern");
        assert_eq!(work_rows(&app).len(), 1, "by holder");
        app.filter.clear();

        query(&mut app, "summary");
        assert_eq!(work_rows(&app).len(), 1, "by note");
    }

    /// The pact-2ol class of defect, which cost this fleet two gate cycles: a
    /// click must land on the row the cursor is over, and `row_at`'s index must
    /// be into the same list `select` indexes into — on a screen where a key
    /// away is `x`.
    #[test]
    fn a_click_on_a_filtered_table_selects_the_lease_under_the_cursor() {
        let tmp = fixture();
        let mut app = app(
            tmp.path(),
            Some("operator"),
            vec![
                entry("orchestrator", "docs/tui.md", 600),
                entry("quern", "src/audit.rs", 120),
                entry("quern", "src/audit_test.rs", 130),
            ],
        );
        // `(all leases)`, so the work pane is the whole table.
        let all = roster_len(&app) - 1;
        select(&mut app, all);
        query(&mut app, "src/");

        let (_, work) = columns(app.content_area, roster_len(&app));
        // The second row of what is DRAWN is src/audit_test.rs — the second row
        // of the unfiltered table is src/audit.rs, which is what an index into
        // the wrong list would select.
        let y = work.y + 1 + 1;
        let index = row_at(&app, work.x, y).expect("a data row");
        select(&mut app, index);
        assert_eq!(app.fleet.selected.as_deref(), Some("src/audit_test.rs"));

        // And past the end of the narrowed table nothing is hit, rather than a
        // row that is only there when the filter is off.
        assert_eq!(row_at(&app, work.x, work.y + 1 + 2), None);
    }

    /// A filter must not silently re-scope the screen. The roster row that is
    /// selected drives the whole right pane, so it stays visible whatever the
    /// query — otherwise typing a path would drop you back to `(all leases)`
    /// and widen the very table you were narrowing.
    #[test]
    fn narrowing_the_roster_never_hides_the_agent_the_work_pane_is_scoped_to() {
        let tmp = fixture();
        let mut app = app(
            tmp.path(),
            Some("operator"),
            vec![entry("orchestrator", "docs/tui.md", 600)],
        );
        let index = roster_rows(&app)
            .iter()
            .position(|a| a.name == "orchestrator")
            .unwrap();
        select(&mut app, index);
        assert_eq!(app.fleet.agent.as_deref(), Some("orchestrator"));

        query(&mut app, "docs/tui.md");
        let names: Vec<&str> = roster_rows(&app).iter().map(|a| a.name.as_str()).collect();
        assert!(
            names.contains(&"orchestrator"),
            "the selected agent stays: {names:?}"
        );
        assert!(
            !names.contains(&"poller"),
            "everyone else narrows: {names:?}"
        );

        // And the cursor is still on that agent, not on whoever moved up into
        // its row number.
        reselect_roster(&mut app);
        assert_eq!(app.fleet.agent.as_deref(), Some("orchestrator"));
        let row = app.fleet.roster.selected().unwrap();
        assert_eq!(roster_rows(&app)[row].name, "orchestrator");
    }

    /// The waiting-on panel never takes the table's header and first rows with
    /// it, however many agents are blocked.
    #[test]
    fn the_waiting_panel_never_squeezes_the_table_out() {
        let right = Rect::new(0, 3, 80, 16);
        let (work, waiting) = stack(right, 40);
        assert!(work.height >= 4, "table kept {} lines", work.height);
        // Title + the capped rows + the one line stating the bound (pact-dwq).
        assert!(waiting.unwrap().height <= 2 + MAX_BLOCKED_ROWS as u16);

        // A terminal with no room at all still shows the leases.
        let (work, waiting) = stack(Rect::new(0, 3, 80, 5), 3);
        assert_eq!(work.height, 5);
        assert!(waiting.is_none());
    }
}
