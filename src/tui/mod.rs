//! Interactive terminal dashboard over what pact manages under `.pact/`
//! (leases, messages) and bd's health. Built on ratatui + its bundled
//! crossterm backend — reimplementing raw-mode terminal handling and a
//! render loop by hand would just be a worse copy of what these already do.
//!
//! This file owns the process-shaped parts only: terminal setup, `App`, the
//! event loop, and the dispatch that hands a frame or a keypress to whichever
//! view is on top of the stack. One screen per module below, so the screens of
//! one epic can be written in parallel instead of queueing on one file.
//!
//! Every module is declared here up front, stub or not: this is the single
//! contention point in the tree, so nothing later has to edit it merely to
//! register itself.

// The navigation spine and the helpers both it and every view share.
mod nav;
mod widgets;

// The read model each view projects from — parsed once per tick, not per view.
mod data;

// One module per screen.
mod activity;
mod detail;
mod fleet;
mod health;
mod messages;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState,
};
use ratatui::{Frame, Terminal};

use crate::beads::BeadsCli;
use crate::doctor;
use crate::lease::{self, LeaseEntry};
use crate::msg;

/// How often the active tab refreshes itself when the user isn't pressing
/// anything — lets a lease or message changed elsewhere show up without a
/// manual 'r'. Only the active tab refreshes on this timer, so idling on
/// Leases doesn't spawn a `bd` subprocess every second for Messages.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// How often the Messages tab's unread badge is refreshed from a tab that
/// isn't Messages. The badge has to be current from ANY tab (an arriving
/// message is invisible otherwise), but the whole point of the rule above is
/// that idling on Leases doesn't spawn `bd` every second — so the badge gets
/// its own, deliberately slower clock. It is checked on the 1 s wake that
/// already happens, so it adds no event-loop wakeups and never touches the
/// animation clock. On the Messages tab it costs nothing at all: the inbox
/// fetch there recomputes the count for free.
const UNREAD_INTERVAL: Duration = Duration::from_secs(10);

/// How often buffered telemetry is pushed to the collector while the ui runs.
///
/// `pact ui` is the only long-lived pact process, and otel's only flush was on
/// the guard at exit — so an eight-hour session exported one batch, timestamped
/// at exit, and a session killed with Ctrl-C or `kill` exported nothing at all
/// (pact-aw7.9). That is exactly backwards for the process a human watches all
/// day. Hung on the 1 s wake that already exists, so it adds no event-loop
/// wakeups; 10 s because a POST is cheaper than a `bd` subprocess but not free,
/// and because it keeps the buffer far below otel's cap either way.
///
/// ponytail: this does not cover SIGKILL, and pact has no signal handler — that
/// needs libc or a hand-declared `extern "C"`, and neither is worth a
/// dependency for the last ten seconds of a dashboard session.
const EXPORT_INTERVAL: Duration = Duration::from_secs(10);

pub fn run(repo_root: PathBuf, agent: Option<String>) -> Result<()> {
    let mut terminal = init_terminal()?;
    let mut app = App::new(repo_root, agent);
    let result = run_event_loop(&mut terminal, &mut app);
    // Always try to restore the terminal, even if the app loop errored —
    // otherwise an error path leaves the caller's shell in raw mode too.
    restore_terminal();
    result
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enabling terminal raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("entering alternate screen")?;
    install_panic_hook();
    Terminal::new(CrosstermBackend::new(io::stdout())).context("initializing terminal backend")
}

/// A panic anywhere in the app loop must still leave the user's shell usable.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal();
        default_hook(panic_info);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tab {
    Leases,
    Messages,
    Doctor,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Leases, Tab::Messages, Tab::Doctor];

    fn label(self) -> &'static str {
        match self {
            Tab::Leases => "Leases",
            Tab::Messages => "Messages",
            Tab::Doctor => "Doctor",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    fn prev(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

struct App {
    repo_root: PathBuf,
    /// The resolved pact identity, if any. `None` means no `--agent`/
    /// `PACT_AGENT` was set — every lease then looks like someone else's, and
    /// the Messages tab has no inbox to show (whose would it be?).
    agent: Option<String>,
    tab: Tab,

    leases: Vec<LeaseEntry>,
    table_state: TableState,
    /// Index into `leases` awaiting a second keypress to force-release.
    confirm_release: Option<usize>,

    /// `Err` if `bd` wasn't found at startup — checked once, not re-probed
    /// on every refresh. The Messages tab shows this error inline instead of
    /// failing the whole UI to launch (Leases stays fully usable).
    bd: std::result::Result<BeadsCli, String>,
    messages: Vec<msg::Message>,
    message_list_state: ListState,
    /// Message ids already marked read this session, so the 1-second refresh
    /// does not rewrite the same read cursor once per tick. A per-tick write is
    /// how a dashboard turns into a storm of pointless work.
    marked_read: std::collections::HashSet<String>,
    /// `Some` while viewing a thread's detail pane instead of the inbox list.
    thread: Option<Vec<msg::Message>>,

    /// `None` until the Doctor tab has been visited (or refreshed) at least
    /// once — same lazy-load pattern as messages, since it also shells out.
    doctor_report: Option<doctor::DoctorReport>,

    /// Unread messages in this agent's inbox, rendered as a badge on the
    /// Messages tab label so new traffic is visible from any tab. Kept fresh
    /// on `UNREAD_INTERVAL` rather than only while the tab is open.
    unread: usize,
    last_unread_refresh: Instant,

    /// Every store under `.pact/` this dashboard reads, parsed once per tick.
    /// Views project from here rather than re-reading a file each — see
    /// `data.rs`.
    data: data::Store,

    status: Option<String>,
    last_refresh: Instant,

    /// Where the tab bar (its inner, post-border area) and the active tab's
    /// content were last drawn — recorded each frame so mouse clicks and
    /// hover both read the exact same rects rendering used, rather than a
    /// second, possibly-drifted approximation of the layout.
    header_area: Rect,
    content_area: Rect,
    /// Tab currently under the mouse cursor (for hover highlighting), if any.
    hovered_tab: Option<Tab>,
    /// Row currently under the mouse cursor in the active tab's list/table.
    hovered_row: Option<usize>,
}

impl App {
    fn new(repo_root: PathBuf, agent: Option<String>) -> Self {
        let bd = BeadsCli::locate().map_err(|e| format!("{e:#}"));
        let now = Instant::now();
        let mut app = App {
            repo_root,
            agent,
            tab: Tab::Leases,
            leases: Vec::new(),
            table_state: TableState::default(),
            confirm_release: None,
            bd,
            messages: Vec::new(),
            message_list_state: ListState::default(),
            marked_read: std::collections::HashSet::new(),
            thread: None,
            doctor_report: None,
            unread: 0,
            // Backdated so the badge is filled in on the first refresh tick
            // instead of a whole UNREAD_INTERVAL after launch — a message
            // already waiting at startup should show up right away.
            last_unread_refresh: now.checked_sub(UNREAD_INTERVAL).unwrap_or(now),
            data: data::Store::default(),
            status: None,
            last_refresh: now,
            header_area: Rect::default(),
            content_area: Rect::default(),
            hovered_tab: None,
            hovered_row: None,
        };
        app.data.refresh(&app.repo_root);
        app.refresh_leases();
        app
    }

    fn next_tab(&mut self) {
        self.set_tab(self.tab.next());
    }

    fn prev_tab(&mut self) {
        self.set_tab(self.tab.prev());
    }

    fn jump_tab(&mut self, tab: Tab) {
        self.set_tab(tab);
    }

    fn set_tab(&mut self, tab: Tab) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.status = None;
        // Stale from the previous tab's list — cleared here rather than left
        // to the next mouse-move event, so a keyboard-driven switch never
        // shows a leftover highlight on an unrelated row.
        self.hovered_row = None;
        self.refresh_active_tab();
    }

    fn refresh_active_tab(&mut self) {
        // Once per tick, before any view asks for anything: the whole point of
        // the read model is that N views cost one parse, not N.
        self.data.refresh(&self.repo_root);
        match self.tab {
            Tab::Leases => self.refresh_leases(),
            Tab::Messages => self.refresh_messages(),
            Tab::Doctor => self.refresh_doctor(),
        }
        self.refresh_unread_if_due();
    }

    /// Keeps the tab-bar unread badge current from tabs that don't poll the
    /// inbox. On Messages this is already a no-op — refresh_messages() above
    /// just reset the clock with the count it fetched — so the only `bd` this
    /// can spawn is one call per UNREAD_INTERVAL while you sit elsewhere.
    fn refresh_unread_if_due(&mut self) {
        // A background status-line write would replace the "press release
        // again to force it" prompt mid-decision; the badge can wait.
        if self.confirm_release.is_some() {
            return;
        }
        if self.last_unread_refresh.elapsed() >= UNREAD_INTERVAL {
            self.refresh_messages();
        }
    }

    fn refresh_doctor(&mut self) {
        self.doctor_report = Some(doctor::checks(&self.repo_root));
        self.last_refresh = Instant::now();
    }

    fn refresh_leases(&mut self) {
        // The one long-lived process in pact, and therefore the one that must not
        // trust a cached HEAD (pact-hxy). `git_history` memoises `head_short` per
        // repo so a batched `acquire`/`release --all` spawns one `git rev-parse`
        // instead of N — sound because HEAD cannot move inside a command that
        // exits. This session does not exit, and it can force-release from the
        // key handler, so the cache is dropped on every tick: staleness is bounded
        // to one refresh interval rather than to the lifetime of the dashboard.
        crate::git_history::forget_head(&self.repo_root);
        // peek, not list: this runs on a refresh timer, and a dashboard that
        // garbage-collects expired locks on every tick is deleting the evidence
        // an operator opened it to look at (pact-rnc.19).
        match lease::peek(&self.repo_root, true) {
            Ok(mut entries) => {
                entries.sort_by(|a, b| a.lease.path.cmp(&b.lease.path));
                self.leases = entries;
                let selected = match self.table_state.selected() {
                    _ if self.leases.is_empty() => None,
                    Some(i) => Some(i.min(self.leases.len() - 1)),
                    None => Some(0),
                };
                self.table_state.select(selected);
            }
            Err(e) => self.status = Some(format!("failed to list leases: {e:#}")),
        }
        self.last_refresh = Instant::now();
    }

    fn refresh_messages(&mut self) {
        self.last_refresh = Instant::now();
        // Set even on the early return below: with no agent there is no inbox to
        // count, and retrying that every second buys nothing.
        self.last_unread_refresh = self.last_refresh;
        let Some(agent) = self.agent.clone() else {
            return; // rendered inline by render_messages; nothing to fetch
        };
        // No `self.bd` gate (pact-as5.3): the inbox is a pact file, so the message
        // panes work in a repo that has never seen the issue tracker.
        match msg::inbox(&self.repo_root, &agent, false) {
            Ok(messages) => {
                self.messages = messages;
                self.unread = self.messages.iter().filter(|m| !m.read).count();
                let selected = match self.message_list_state.selected() {
                    _ if self.messages.is_empty() => None,
                    Some(i) => Some(i.min(self.messages.len() - 1)),
                    None => Some(0),
                };
                self.message_list_state.select(selected);
                self.mark_selected_read();
            }
            Err(e) => self.status = Some(format!("failed to fetch inbox: {e:#}")),
        }
    }

    /// Mark the message under the cursor as read.
    ///
    /// The dashboard IS the human's inbox, and that was the gap: 41 of 85
    /// messages in one fleet run were addressed to `human`, who never runs
    /// `pact msg read`, so `pact msg sent` reported every one of them unread
    /// forever. The protocol tells agents "confirm, don't re-send: `pact msg
    /// sent` shows whether the recipient has read it" — and for the single most
    /// important recipient that instruction always answered no, which is how an
    /// inbox reaches sixty entries nobody can triage (pact-4tj).
    ///
    /// Selection, not display: scrolling past a line is reading it, but merely
    /// opening the tab is not, and marking the whole list read on arrival would
    /// destroy the unread markers that make the list worth having.
    fn mark_selected_read(&mut self) {
        let Some(agent) = self.agent.clone() else {
            return;
        };
        let Some(m) = self
            .message_list_state
            .selected()
            .and_then(|i| self.messages.get(i))
        else {
            return;
        };
        // Already read, or already done this session: no repeated write.
        if m.read || !self.marked_read.insert(m.id.clone()) {
            return;
        }
        let id = m.id.clone();
        if let Err(e) = msg::mark_read_by_id(&self.repo_root, &agent, &id) {
            // Non-fatal and non-blocking: a dashboard that cannot update read
            // state is still a dashboard. Retrying every tick would be the
            // subprocess storm this guard exists to prevent, so the id stays in
            // the set either way.
            self.status = Some(format!("could not mark {id} read: {e:#}"));
        }
    }

    fn open_selected_thread(&mut self) {
        let Some(index) = self.message_list_state.selected() else {
            return;
        };
        let Some(id) = self.messages.get(index).map(|m| m.id.clone()) else {
            return;
        };
        let Some(agent) = self.agent.clone() else {
            return;
        };
        match msg::read_thread(&self.repo_root, &agent, &id) {
            Ok(thread) => {
                self.thread = Some(thread);
                self.refresh_messages(); // pick up the now-read marker in the list behind it
            }
            Err(e) => self.status = Some(format!("failed to read thread: {e:#}")),
        }
    }

    fn close_thread(&mut self) {
        self.thread = None;
    }

    fn is_mine(&self, entry: &LeaseEntry) -> bool {
        self.agent.as_deref() == Some(entry.lease.agent.as_str())
    }

    fn move_lease_selection(&mut self, delta: isize) {
        if self.leases.is_empty() {
            return;
        }
        let len = self.leases.len() as isize;
        let current = self.table_state.selected().unwrap_or(0) as isize;
        self.table_state
            .select(Some((current + delta).rem_euclid(len) as usize));
        self.confirm_release = None;
    }

    fn move_message_selection(&mut self, delta: isize) {
        if self.messages.is_empty() {
            return;
        }
        let len = self.messages.len() as isize;
        let current = self.message_list_state.selected().unwrap_or(0) as isize;
        self.message_list_state
            .select(Some((current + delta).rem_euclid(len) as usize));
        self.mark_selected_read();
    }

    /// Click on the leases table: select whichever row is under `y`, if any.
    fn click_lease_row(&mut self, y: u16) {
        if let Some(index) = row_at(self.content_area, y, 1, self.table_state.offset()) {
            if index < self.leases.len() {
                self.table_state.select(Some(index));
                self.confirm_release = None;
            }
        }
    }

    /// Click on the messages list: select whichever row is under `y`, if any
    /// (only while looking at the list — a click during the thread detail
    /// view is a no-op, there's nothing there to select).
    fn click_message_row(&mut self, y: u16) {
        if let Some(index) = row_at(self.content_area, y, 0, self.message_list_state.offset()) {
            if index < self.messages.len() {
                self.message_list_state.select(Some(index));
                self.mark_selected_read();
            }
        }
    }

    /// First press on someone else's lease asks for confirmation; a second
    /// press on the same row forces the release. A press on your own lease
    /// releases it immediately — no confirmation needed for your own claim.
    fn handle_release_key(&mut self) {
        let Some(index) = self.table_state.selected() else {
            return;
        };
        let Some(entry) = self.leases.get(index) else {
            return;
        };
        let path = entry.lease.path.clone();

        if self.confirm_release == Some(index) {
            let agent = self
                .agent
                .clone()
                .unwrap_or_else(|| entry.lease.agent.clone());
            match lease::release(&self.repo_root, &agent, &path, true) {
                // A displaced holder means force actually took the lease off
                // someone else. Naming them is the whole value of the force path
                // here: the ui is where a human does this, and "who did I just
                // step on" is the one fact they need to go tell that agent.
                Ok(outcome) => {
                    self.status = Some(match outcome.displaced() {
                        Some(displaced) => {
                            format!("force-released {path} (was held by {displaced})")
                        }
                        None => format!("released {path}"),
                    });
                }
                Err(e) => self.status = Some(format!("release failed: {e:#}")),
            }
            self.confirm_release = None;
            self.refresh_leases();
        } else if self.is_mine(entry) {
            let agent = self.agent.clone().expect("is_mine implies agent is set");
            match lease::release(&self.repo_root, &agent, &path, false) {
                // Never Some without force: you can only displace yourself,
                // which release() reports as None.
                Ok(_) => self.status = Some(format!("released {path}")),
                Err(e) => self.status = Some(format!("release failed: {e:#}")),
            }
            self.refresh_leases();
        } else {
            self.confirm_release = Some(index);
            self.status = Some(format!(
                "held by {} — press release again to force it, or Esc to cancel",
                entry.lease.agent
            ));
        }
    }

    fn cancel_confirm(&mut self) {
        if self.confirm_release.take().is_some() {
            self.status = None;
        }
    }
}

fn run_event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    let mut last_export = Instant::now();
    loop {
        terminal.draw(|frame| draw(frame, app))?;

        // See EXPORT_INTERVAL. A no-op in the default build, and a no-op in the
        // otel build until something is actually configured.
        if last_export.elapsed() >= EXPORT_INTERVAL {
            last_export = Instant::now();
            crate::otel::flush_now();
        }

        // The DATA refresh is the only clock left, and it is the only one this
        // loop may ever wake on: refresh_active_tab() shells out to `bd`/doctor
        // and re-parses the whole event and message store, so a shorter timeout
        // here is ~10 subprocesses and ~10 full re-parses a second. A zero
        // remaining wakes immediately, refreshes below, and resets the clock —
        // so it cannot spin either.
        let timeout = REFRESH_INTERVAL.saturating_sub(app.last_refresh.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if is_quit(key.code, key.modifiers) {
                        return Ok(());
                    }
                    handle_key(app, key.code);
                }
                Event::Mouse(mouse) => handle_mouse(app, mouse),
                _ => {}
            }
        } else if app.last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh_active_tab();
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Tab => return app.next_tab(),
        KeyCode::BackTab => return app.prev_tab(),
        KeyCode::Char('1') => return app.jump_tab(Tab::Leases),
        KeyCode::Char('2') => return app.jump_tab(Tab::Messages),
        KeyCode::Char('3') => return app.jump_tab(Tab::Doctor),
        _ => {}
    }
    match app.tab {
        Tab::Leases => handle_leases_key(app, code),
        Tab::Messages => handle_messages_key(app, code),
        Tab::Doctor => handle_doctor_key(app, code),
    }
}

fn handle_leases_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Down | KeyCode::Char('j') => app.move_lease_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_lease_selection(-1),
        KeyCode::Char('r') => app.refresh_leases(),
        KeyCode::Enter | KeyCode::Char('d') => app.handle_release_key(),
        KeyCode::Esc | KeyCode::Char('n') => app.cancel_confirm(),
        _ => {}
    }
}

fn handle_messages_key(app: &mut App, code: KeyCode) {
    if app.thread.is_some() {
        if matches!(code, KeyCode::Esc | KeyCode::Char('b')) {
            app.close_thread();
        }
        return;
    }
    match code {
        KeyCode::Down | KeyCode::Char('j') => app.move_message_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_message_selection(-1),
        KeyCode::Char('r') => app.refresh_messages(),
        KeyCode::Enter => app.open_selected_thread(),
        _ => {}
    }
}

fn handle_doctor_key(app: &mut App, code: KeyCode) {
    if code == KeyCode::Char('r') {
        app.refresh_doctor();
    }
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => handle_click(app, mouse.column, mouse.row),
        MouseEventKind::ScrollDown => handle_scroll(app, 1),
        MouseEventKind::ScrollUp => handle_scroll(app, -1),
        MouseEventKind::Moved => update_hover(app, mouse.column, mouse.row),
        _ => {}
    }
}

fn handle_click(app: &mut App, x: u16, y: u16) {
    if let Some(tab) = tab_at(app.header_area, app.unread, x, y) {
        app.jump_tab(tab);
        return;
    }
    if !rect_contains(app.content_area, x, y) {
        return;
    }
    match app.tab {
        Tab::Leases => app.click_lease_row(y),
        Tab::Messages if app.thread.is_none() => app.click_message_row(y),
        _ => {}
    }
}

fn handle_scroll(app: &mut App, delta: isize) {
    match app.tab {
        Tab::Leases => app.move_lease_selection(delta),
        Tab::Messages if app.thread.is_none() => app.move_message_selection(delta),
        _ => {}
    }
}

/// Updates hover state so the header/list can highlight whatever's directly
/// under the cursor, before a click commits to anything. Requested after a
/// user found the tab bar unresponsive on their terminal: with nothing to
/// show which zone a click would land in, a slight hit-test mismatch (the
/// old equal-thirds approximation vs. the tabs' real, unequal widths) just
/// looked like "mouse doesn't work" rather than "clicked the wrong spot".
fn update_hover(app: &mut App, x: u16, y: u16) {
    app.hovered_tab = tab_at(app.header_area, app.unread, x, y);

    app.hovered_row = if rect_contains(app.content_area, x, y) {
        match app.tab {
            Tab::Leases => row_at(app.content_area, y, 1, app.table_state.offset())
                .filter(|i| *i < app.leases.len()),
            Tab::Messages if app.thread.is_none() => {
                row_at(app.content_area, y, 0, app.message_list_state.offset())
                    .filter(|i| *i < app.messages.len())
            }
            _ => None,
        }
    } else {
        None
    };
}

fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height
}

/// The exact rects the tab bar renders into: one per tab, in `Tab::ALL`
/// order, each `" Label "`-wide, separated by a 1-column (non-clickable)
/// gap. Used for both rendering and hit-testing so the two can never drift
/// apart the way an independent approximation (e.g. equal-width zones) can.
fn tab_rects(header_area: Rect, unread: usize) -> Vec<Rect> {
    let mut constraints = Vec::with_capacity(Tab::ALL.len() * 2 - 1);
    for (i, tab) in Tab::ALL.iter().enumerate() {
        constraints.push(Constraint::Length(tab_width(*tab, unread)));
        if i + 1 < Tab::ALL.len() {
            constraints.push(Constraint::Length(1)); // gap between tabs
        }
    }
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(header_area)
        .iter()
        .step_by(2) // skip the gap chunks, keep only the tab-label chunks
        .copied()
        .collect()
}

/// The label as it is actually rendered, unread badge included. Everything
/// that measures a tab goes through this one function, so widening "Messages"
/// to "Messages (3)" widens its rect too — a badge that only rendering knew
/// about would shift every later tab out from under its own hit-box.
fn tab_label(tab: Tab, unread: usize) -> String {
    match tab {
        Tab::Messages if unread > 0 => format!("{} ({unread})", tab.label()),
        _ => tab.label().to_string(),
    }
}

fn tab_width(tab: Tab, unread: usize) -> u16 {
    tab_label(tab, unread).chars().count() as u16 + 2 // " Label " padding
}

/// Which tab a click/hover at `(x, y)` lands on, using the same rects
/// `tab_rects` hands to rendering.
fn tab_at(header_area: Rect, unread: usize, x: u16, y: u16) -> Option<Tab> {
    tab_rects(header_area, unread)
        .into_iter()
        .zip(Tab::ALL)
        .find(|(rect, _)| rect_contains(*rect, x, y))
        .map(|(_, tab)| tab)
}

/// Which row (as an index into the underlying `Vec`) a click at `y` (inside
/// `content_area`) lands on, given how many header rows the widget draws
/// before its data (1 for the leases table's column header, 0 for the
/// messages list) and how far the list is currently scrolled.
fn row_at(content_area: Rect, y: u16, header_rows: u16, offset: usize) -> Option<usize> {
    let first_data_row = content_area.y + header_rows;
    if y < first_data_row {
        return None;
    }
    Some(offset + (y - first_data_row) as usize)
}

fn is_quit(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q'))
        || (matches!(code, KeyCode::Char('c')) && modifiers.contains(KeyModifiers::CONTROL))
}

fn draw(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Recorded so row_at/rect_contains/click_lease_row hit-test against the exact
    // rect the list was rendered into — the same discipline tab_rects follows for
    // the header, and the reason a click can never land on a row it did not hit.
    let list_area = chunks[1];
    app.content_area = list_area;

    render_header(frame, chunks[0], app);
    match app.tab {
        Tab::Leases => render_leases(frame, list_area, app),
        Tab::Messages => render_messages(frame, list_area, app),
        Tab::Doctor => render_doctor(frame, list_area, app),
    }
    render_status(frame, chunks[2], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &mut App) {
    let agent_label = app.agent.as_deref().unwrap_or("(none — set PACT_AGENT)");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" pact ui — agent: {agent_label} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Recorded so hit-testing (tab_at) and hover (update_hover) use the
    // exact same area rendering just used — see tab_rects.
    app.header_area = inner;

    for (tab, rect) in Tab::ALL.into_iter().zip(tab_rects(inner, app.unread)) {
        let selected = tab == app.tab;
        let hovered = !selected && app.hovered_tab == Some(tab);
        let style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if hovered {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else if tab == Tab::Messages && app.unread > 0 {
            // The count alone is easy to miss while you're reading another
            // tab, which is the exact situation this badge exists for.
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(format!(" {} ", tab_label(tab, app.unread))).style(style),
            rect,
        );
    }
}

fn render_leases(frame: &mut Frame, area: Rect, app: &App) {
    if app.leases.is_empty() {
        frame.render_widget(
            Paragraph::new("no active leases — press r to refresh")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    // pact-rnc.10: no raw "Remaining" countdown. A four-digit second count next
    // to a seconds-old lease read as "this long lease" and got a live agent's
    // claim force-released — and in here --force is two keystrokes away. Age and
    // state say what an operator needs; both come from lease.rs, so this table
    // and `pact lease ls` cannot disagree.
    let header = Row::new(vec!["Path", "Held by", "Age", "State", "Note"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
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
        // label and the one an operator must never have truncated out from under
        // them — a half-read state is how pact-rnc.10 happened.
        Constraint::Length(26),
        Constraint::Percentage(20),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    frame.render_stateful_widget(table, area, &mut app.table_state.clone());
}

fn lease_row<'a>(app: &App, index: usize, entry: &'a LeaseEntry) -> Row<'a> {
    let agent_style = if app.is_mine(entry) {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    };
    let state_style = match entry.state() {
        "expired" => Style::default().fg(Color::Red),
        "stale" => Style::default().fg(Color::Yellow),
        // A suspect holder is the case this dashboard exists to catch: seven of
        // ten crucible agents stalled, and to a green `active` row they looked
        // fine. Yellow like `stale`, because both mean "look at this" — the label
        // is what says which (pact-mqw.6).
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

    // Selection's own reversed style is already a strong indicator; hover
    // only adds anything on rows that aren't already selected.
    if is_hovered_not_selected(app.hovered_row, app.table_state.selected(), index) {
        row.style(hover_style())
    } else {
        row
    }
}

fn render_messages(frame: &mut Frame, area: Rect, app: &App) {
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
    if let Some(thread) = &app.thread {
        render_thread(frame, area, thread);
        return;
    }
    if app.messages.is_empty() {
        frame.render_widget(
            Paragraph::new("inbox empty — press r to refresh")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| message_list_item(app, i, m))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut app.message_list_state.clone());
}

fn message_list_item<'a>(app: &App, index: usize, message: &'a msg::Message) -> ListItem<'a> {
    let marker = if message.read { "  " } else { "* " };
    let subject = message.subject.as_deref().unwrap_or("(no subject)");
    let mut style = if message.read {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    if is_hovered_not_selected(app.hovered_row, app.message_list_state.selected(), index) {
        style = style.patch(hover_style());
    }
    ListItem::new(format!("{marker}{}  {subject}", message.id)).style(style)
}

/// Whether `index` is hovered but not the current selection — selection's
/// own reversed style is already a strong enough indicator on its own.
fn is_hovered_not_selected(hovered: Option<usize>, selected: Option<usize>, index: usize) -> bool {
    hovered == Some(index) && selected != Some(index)
}

fn hover_style() -> Style {
    Style::default().bg(Color::Rgb(50, 50, 65))
}

fn render_thread(frame: &mut Frame, area: Rect, thread: &[msg::Message]) {
    let text = thread
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
        .join("\n---\n");
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" thread (esc: back) "),
        ),
        area,
    );
}

fn render_doctor(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = &app.doctor_report else {
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
    let title = format!(" {} ", crate::doctor::summary(report));
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let help = match app.tab {
        Tab::Leases => {
            "j/k: move  r: refresh  enter/d: release  esc: cancel  tab: switch  1/2/3: jump  q: quit"
        }
        Tab::Messages if app.thread.is_some() => "esc: back  tab: switch  1/2/3: jump  q: quit",
        Tab::Messages => {
            "j/k: move  r: refresh  enter: open thread  tab: switch  1/2/3: jump  q: quit"
        }
        Tab::Doctor => "r: refresh  tab: switch  1/2/3: jump  q: quit",
    };
    let line = match &app.status {
        Some(status) => format!("{status}   ({help})"),
        None => help.to_string(),
    };
    let style = if app.confirm_release.is_some() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(Paragraph::new(Line::from(line)).style(style), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quits_on_q_or_ctrl_c_only() {
        assert!(is_quit(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(is_quit(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!is_quit(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(!is_quit(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!is_quit(KeyCode::Enter, KeyModifiers::NONE));
    }

    fn entry(agent: &str, path: &str, expired: bool) -> LeaseEntry {
        LeaseEntry {
            lease: lease::LeaseInfo {
                agent: agent.to_string(),
                path: path.to_string(),
                acquired_at: "2026-01-01T00:00:00Z".to_string(),
                ttl_secs: 900,
                note: None,
                branch: None,
                worktree: None,
                invoked_from: None,
                content_hash: None,
                extra: Default::default(),
            },
            age_secs: 10,
            remaining_secs: 890,
            expired,
            holder_silent_secs: None,
            suspect: false,
        }
    }

    fn message(id: &str, subject: &str, read: bool) -> msg::Message {
        msg::Message {
            id: id.to_string(),
            thread: id.to_string(),
            from: "agent-b".to_string(),
            to: "agent-a".to_string(),
            subject: Some(subject.to_string()),
            body: format!("body of {id}"),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            read,
            read_by: Vec::new(),
            notice: false,
        }
    }

    fn app_with(agent: Option<&str>, leases: Vec<LeaseEntry>) -> App {
        App {
            repo_root: PathBuf::new(),
            agent: agent.map(str::to_string),
            tab: Tab::Leases,
            leases,
            table_state: TableState::default().with_selected(Some(0)),
            confirm_release: None,
            bd: Err("bd (beads) not found on PATH".to_string()),
            messages: Vec::new(),
            message_list_state: ListState::default().with_selected(Some(0)),
            marked_read: std::collections::HashSet::new(),
            thread: None,
            doctor_report: None,
            unread: 0,
            last_unread_refresh: Instant::now(),
            data: data::Store::default(),
            status: None,
            last_refresh: Instant::now(),
            header_area: Rect::new(0, 0, 80, 1),
            content_area: Rect::new(0, 3, 80, 8),
            hovered_tab: None,
            hovered_row: None,
        }
    }

    #[test]
    fn move_selection_wraps_both_ways() {
        let mut app = app_with(
            None,
            vec![entry("a", "one", false), entry("a", "two", false)],
        );
        app.move_lease_selection(1);
        assert_eq!(app.table_state.selected(), Some(1));
        app.move_lease_selection(1);
        assert_eq!(app.table_state.selected(), Some(0));
        app.move_lease_selection(-1);
        assert_eq!(app.table_state.selected(), Some(1));
    }

    #[test]
    fn is_mine_requires_matching_agent() {
        let app = app_with(Some("agent-a"), vec![]);
        assert!(app.is_mine(&entry("agent-a", "x", false)));
        assert!(!app.is_mine(&entry("agent-b", "x", false)));

        let no_identity = app_with(None, vec![]);
        assert!(!no_identity.is_mine(&entry("agent-a", "x", false)));
    }

    #[test]
    fn tab_at_agrees_with_tab_rects_exact_geometry() {
        let header = Rect::new(0, 0, 90, 1);
        // Including badged widths: "Messages (3)" and the two-digit "(12)" are
        // wider labels, and hit-testing has to move with them.
        for unread in [0, 3, 12] {
            let rects = tab_rects(header, unread);
            assert_eq!(rects.len(), 3);

            // Every point inside a tab's own rendered rect resolves back to it —
            // not an equal-width approximation, the exact rect rendering used.
            for (tab, rect) in Tab::ALL.into_iter().zip(rects.iter()) {
                assert_eq!(tab_at(header, unread, rect.x, rect.y), Some(tab));
                assert_eq!(
                    tab_at(header, unread, rect.x + rect.width - 1, rect.y),
                    Some(tab)
                );
                assert_eq!(
                    rect.width,
                    tab_label(tab, unread).chars().count() as u16 + 2
                );
            }

            // The 1-column gap between tabs belongs to neither (not clickable).
            let gap_x = rects[0].x + rects[0].width;
            assert!(gap_x < rects[1].x, "expected a gap between tab rects");
            assert_eq!(tab_at(header, unread, gap_x, 0), None);

            // Empty header space past the last tab matches nothing, rather than
            // falling back to "closest tab" the way an equal-zone division would.
            let last = rects.last().unwrap();
            assert_eq!(tab_at(header, unread, last.x + last.width + 10, 0), None);
        }
    }

    #[test]
    fn unread_badge_widens_the_messages_tab_and_shifts_doctor_with_it() {
        let header = Rect::new(0, 0, 90, 1);
        let plain = tab_rects(header, 0);
        let badged = tab_rects(header, 3);

        assert_eq!(badged[0], plain[0], "Leases sits before Messages");
        assert_eq!(
            badged[1].width,
            plain[1].width + 4, // " (3)"
            "the badge must be part of the measured label"
        );
        // The tab AFTER the badge is the one a naive fix breaks: rendering
        // pushes it right while hit-testing keeps the old rect.
        assert_eq!(badged[2].x, plain[2].x + 4);
        assert_eq!(tab_at(header, 3, badged[2].x, 0), Some(Tab::Doctor));
        assert_eq!(tab_at(header, 3, plain[2].x, 0), Some(Tab::Messages));
    }

    #[test]
    fn header_renders_the_unread_count_from_another_tab() {
        use ratatui::backend::TestBackend;

        let mut app = app_with(Some("agent-a"), vec![]);
        app.tab = Tab::Leases; // the tab the human was on in pact-rnc.14
        app.unread = 3;

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(render_to_string(&terminal).contains("Messages (3)"));

        // No badge at all when there's nothing unread.
        app.unread = 0;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("Messages"));
        assert!(!rendered.contains("Messages ("));
    }

    /// The ui exports on a timer, and the timer has to be readable off the two
    /// clocks that already exist. Too short and every animation frame carries a
    /// POST; longer than a coffee break and `pact ui` is back to what
    /// pact-aw7.9 found — one batch at exit, nothing at all on Ctrl-C, and a
    /// whole session's gauges collapsed onto the exit timestamp.
    #[test]
    fn telemetry_is_exported_on_a_clock_the_event_loop_already_wakes_for() {
        assert!(EXPORT_INTERVAL >= REFRESH_INTERVAL);
        assert!(EXPORT_INTERVAL <= Duration::from_secs(60));
    }

    #[test]
    fn the_unread_badge_refreshes_on_its_own_slower_clock() {
        // The badge must not turn the Leases tab into a once-a-second `bd`
        // spawner, so it has a clock of its own that gates the inbox fetch.
        assert!(UNREAD_INTERVAL >= REFRESH_INTERVAL * 5);

        let mut app = app_with(Some("agent-a"), vec![]);
        // bd is Err here, so refresh_messages() sets last_refresh and returns
        // without spawning anything — last_refresh is the observable for "the
        // inbox fetch ran".
        app.last_refresh = Instant::now() - Duration::from_secs(60);
        app.last_unread_refresh = Instant::now();
        app.refresh_unread_if_due();
        assert!(
            app.last_refresh.elapsed() >= Duration::from_secs(60),
            "not due yet: the inbox must not be fetched"
        );

        app.last_unread_refresh = Instant::now() - UNREAD_INTERVAL;
        app.refresh_unread_if_due();
        assert!(app.last_refresh.elapsed() < Duration::from_secs(1));

        // Armed for a force-release: the fetch would overwrite the confirmation
        // prompt in the status line, so it waits.
        app.last_refresh = Instant::now() - Duration::from_secs(60);
        app.last_unread_refresh = Instant::now() - UNREAD_INTERVAL;
        app.confirm_release = Some(0);
        app.refresh_unread_if_due();
        assert!(app.last_refresh.elapsed() >= Duration::from_secs(60));
    }

    #[test]
    fn row_at_accounts_for_header_rows_and_scroll_offset() {
        let content = Rect::new(0, 3, 80, 8);
        // leases table: 1 header row, no scroll yet
        assert_eq!(row_at(content, 3, 1, 0), None); // clicked the column header itself
        assert_eq!(row_at(content, 4, 1, 0), Some(0));
        assert_eq!(row_at(content, 5, 1, 0), Some(1));
        // scrolled down by 2: same click now lands on a later row
        assert_eq!(row_at(content, 4, 1, 2), Some(2));
        // messages list: no header row
        assert_eq!(row_at(content, 3, 0, 0), Some(0));
    }

    #[test]
    fn clicking_a_lease_row_selects_it() {
        let mut app = app_with(
            Some("agent-a"),
            vec![
                entry("agent-a", "one.rs", false),
                entry("agent-a", "two.rs", false),
                entry("agent-a", "three.rs", false),
            ],
        );
        app.content_area = Rect::new(0, 3, 80, 8);
        // row 0 ("one.rs") is at content_area.y + 1 (header row) + 0
        app.click_lease_row(4);
        assert_eq!(app.table_state.selected(), Some(0));
        app.click_lease_row(6);
        assert_eq!(app.table_state.selected(), Some(2));
        // out-of-range click is a no-op, not a panic or a bogus selection
        app.click_lease_row(50);
        assert_eq!(app.table_state.selected(), Some(2));
    }

    #[test]
    fn clicking_a_message_row_selects_it() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.content_area = Rect::new(0, 3, 80, 8);
        app.messages = vec![
            message("m1", "first", false),
            message("m2", "second", false),
        ];
        app.click_message_row(4);
        assert_eq!(app.message_list_state.selected(), Some(1));
    }

    #[test]
    fn clicking_the_header_switches_tabs() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.header_area = Rect::new(0, 0, 90, 1);
        app.content_area = Rect::new(0, 1, 90, 8);
        assert_eq!(app.tab, Tab::Leases);

        let doctor_rect = tab_rects(app.header_area, app.unread)[2];
        handle_click(&mut app, doctor_rect.x, doctor_rect.y);
        assert_eq!(app.tab, Tab::Doctor);

        // Same click path with an unread badge widening the Messages label:
        // handle_click reads app.unread, so the rects it tests against are the
        // ones that were rendered, not the unbadged ones.
        app.unread = 3;
        let shifted_doctor = tab_rects(app.header_area, app.unread)[2];
        assert_ne!(shifted_doctor.x, doctor_rect.x);
        app.jump_tab(Tab::Leases);
        handle_click(&mut app, shifted_doctor.x, shifted_doctor.y);
        assert_eq!(app.tab, Tab::Doctor);
    }

    #[test]
    fn hovering_a_tab_sets_hovered_tab_without_switching() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.header_area = Rect::new(0, 0, 90, 1);
        app.content_area = Rect::new(0, 1, 90, 8);

        let messages_rect = tab_rects(app.header_area, app.unread)[1];
        update_hover(&mut app, messages_rect.x, messages_rect.y);
        assert_eq!(app.hovered_tab, Some(Tab::Messages));
        assert_eq!(app.tab, Tab::Leases); // hover alone never switches tabs

        // moving off the header clears it
        update_hover(&mut app, 0, 50);
        assert_eq!(app.hovered_tab, None);
    }

    #[test]
    fn hovering_a_lease_row_sets_hovered_row() {
        let mut app = app_with(
            Some("agent-a"),
            vec![
                entry("agent-a", "one.rs", false),
                entry("agent-a", "two.rs", false),
            ],
        );
        app.content_area = Rect::new(0, 3, 80, 8);

        update_hover(&mut app, 0, 5); // second data row (first is the column header)
        assert_eq!(app.hovered_row, Some(1));

        // switching tabs clears a stale hover from the previous list
        app.next_tab();
        assert_eq!(app.hovered_row, None);
    }

    #[test]
    fn scrolling_moves_the_active_tabs_selection() {
        let mut app = app_with(
            Some("agent-a"),
            vec![
                entry("agent-a", "one.rs", false),
                entry("agent-a", "two.rs", false),
            ],
        );
        handle_scroll(&mut app, 1);
        assert_eq!(app.table_state.selected(), Some(1));
        handle_scroll(&mut app, -1);
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn releasing_someone_elses_lease_requires_confirmation_first() {
        let mut app = app_with(Some("agent-a"), vec![entry("agent-b", "shared.rs", false)]);
        app.handle_release_key();
        assert_eq!(app.confirm_release, Some(0));
        // lease is untouched: repo_root is bogus so a real release would
        // have errored into `status`, not silently succeeded.
        assert!(app.status.as_deref().unwrap_or("").contains("force it"));
    }

    #[test]
    fn tab_cycles_forward_and_backward_through_all_three() {
        let mut app = app_with(Some("agent-a"), vec![]);
        assert_eq!(app.tab, Tab::Leases);

        app.next_tab();
        assert_eq!(app.tab, Tab::Messages);
        app.next_tab();
        assert_eq!(app.tab, Tab::Doctor);
        app.next_tab();
        assert_eq!(app.tab, Tab::Leases);

        app.prev_tab();
        assert_eq!(app.tab, Tab::Doctor);
    }

    #[test]
    fn jump_tab_goes_directly_to_the_requested_tab() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.jump_tab(Tab::Doctor);
        assert_eq!(app.tab, Tab::Doctor);
        app.jump_tab(Tab::Leases);
        assert_eq!(app.tab, Tab::Leases);
    }

    #[test]
    fn switching_tabs_does_not_itself_close_an_open_thread() {
        // Only closing the thread view (Esc) does that — exercised in
        // close_thread_returns_to_the_list below.
        let mut app = app_with(Some("agent-a"), vec![]);
        app.thread = Some(vec![message("m1", "hi", true)]);
        app.next_tab();
        app.jump_tab(Tab::Messages);
        assert!(app.thread.is_some());
    }

    #[test]
    fn close_thread_returns_to_the_list() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.thread = Some(vec![message("m1", "hi", true)]);
        app.close_thread();
        assert!(app.thread.is_none());
    }

    #[test]
    fn message_list_item_marks_unread_with_asterisk() {
        let app = app_with(Some("agent-a"), vec![]);
        let unread_msg = message("m1", "hello", false);
        let read_msg = message("m2", "world", true);
        let unread = message_list_item(&app, 0, &unread_msg);
        let read = message_list_item(&app, 0, &read_msg);
        // ListItem doesn't expose its text for direct comparison, so compare
        // Debug output instead of reaching into private state.
        assert_ne!(format!("{unread:?}"), format!("{read:?}"));
    }

    #[test]
    fn renders_leases_table_without_panicking() {
        use ratatui::backend::TestBackend;

        let mut app = app_with(
            Some("agent-a"),
            vec![
                entry("agent-a", "mine.rs", false),
                entry("agent-b", "theirs.rs", true),
            ],
        );
        app.table_state.select(Some(0));

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("pact ui"));
        assert!(rendered.contains("agent-a"));
        assert!(rendered.contains("mine.rs"));
        assert!(rendered.contains("theirs.rs"));
        assert!(rendered.contains("expired"));
        assert!(rendered.contains("active"));
    }

    /// pact-rnc.10 on the surface where the incident happened: the dashboard is
    /// where a force-release gets decided, and it used to render the misreadable
    /// `80s  3520s  active`.
    #[test]
    fn leases_table_shows_age_and_state_never_a_raw_countdown() {
        use ratatui::backend::TestBackend;

        let fresh = LeaseEntry {
            age_secs: 80,
            remaining_secs: 3520,
            ..entry("animator", "docs/m.md", false)
        };
        let stale = LeaseEntry {
            age_secs: 910,
            remaining_secs: -10,
            ..entry("gone", "src/z.rs", false)
        };
        let mut app = app_with(Some("animator"), vec![fresh, stale]);
        app.table_state.select(Some(0));

        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("1m20s"), "age must be human: {rendered}");
        assert!(
            !rendered.contains("3520"),
            "a raw ttl countdown is the whole defect: {rendered}"
        );
        assert!(!rendered.contains("Remaining"), "{rendered}");
        assert!(
            rendered.contains("stale (reclaimable in 20s)"),
            "the third band exists in the TUI too: {rendered}"
        );
    }

    #[test]
    fn messages_tab_shows_bd_missing_error_inline() {
        use ratatui::backend::TestBackend;

        let mut app = app_with(Some("agent-a"), vec![]);
        app.tab = Tab::Messages;
        // app_with already seeds `bd` as Err, matching "bd not on PATH".

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("Messages"));
        assert!(rendered.contains("bd (beads) not found"));
    }

    #[test]
    fn messages_tab_renders_thread_detail_when_open() {
        use ratatui::backend::TestBackend;

        let mut app = app_with(Some("agent-a"), vec![]);
        app.tab = Tab::Messages;
        app.bd = Ok(BeadsCli { binary: "bd" });
        app.thread = Some(vec![message("msg-1", "renamed foo()", true)]);

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("thread"));
        assert!(rendered.contains("renamed foo()"));
        assert!(rendered.contains("body of msg-1"));
    }

    #[test]
    fn doctor_tab_prompts_before_first_refresh_then_shows_checks() {
        use ratatui::backend::TestBackend;

        let mut app = app_with(Some("agent-a"), vec![]);
        app.tab = Tab::Doctor;

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(render_to_string(&terminal).contains("press r"));

        app.doctor_report = Some(doctor::DoctorReport {
            healthy: false,
            checks: vec![doctor::DoctorCheck {
                name: "Beads CLI",
                ok: false,
                warn: false,
                detail: "bd not found on PATH".to_string(),
            }],
        });
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("Beads CLI"));
        assert!(rendered.contains("bd not found on PATH"));
        assert!(rendered.contains("some checks failed"));
    }

    /// The list owns the whole content chunk at every size, and clicks land on
    /// the row they hit. This used to be two tests because a decoration carved
    /// columns out of the right edge and the geometry differed above and below
    /// its threshold; the invariant it was really guarding — content_area IS the
    /// rect the list rendered into — is what survives.
    #[test]
    fn the_list_owns_the_content_chunk_and_clicks_map_to_rows_at_any_size() {
        use ratatui::backend::TestBackend;

        let mut app = app_with(
            Some("agent-a"),
            vec![
                entry("agent-a", "one.rs", false),
                entry("agent-a", "two.rs", false),
                entry("agent-a", "three.rs", false),
            ],
        );

        for (cols, rows) in [(100u16, 24u16), (69, 24), (100, 15)] {
            let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();

            assert_eq!(app.content_area.width, cols, "at {cols}x{rows}");
            assert_eq!(app.content_area.x, 0, "at {cols}x{rows}");
            assert!(render_to_string(&terminal).contains("one.rs"));

            // Header row, then data rows — the same arithmetic row_at does.
            app.click_lease_row(app.content_area.y);
            assert_eq!(app.table_state.selected(), Some(0)); // column header: no change
            app.click_lease_row(app.content_area.y + 3);
            assert_eq!(app.table_state.selected(), Some(2), "at {cols}x{rows}");
            app.table_state.select(Some(0));
        }
    }

    #[test]
    fn arming_a_force_release_and_cancelling_it() {
        let mut app = app_with(Some("agent-a"), vec![entry("agent-b", "shared.rs", false)]);
        app.handle_release_key();
        assert_eq!(app.confirm_release, Some(0));

        app.cancel_confirm();
        assert_eq!(app.confirm_release, None);
        assert_eq!(app.status, None, "the prompt must go with the armed state");
    }

    #[test]
    fn releasing_your_own_lease_needs_no_confirmation() {
        // An empty repo_root would resolve relative to the CWD and could touch
        // the developer's own .pact/leases — a scratch dir keeps this hermetic.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(Some("agent-a"), vec![entry("agent-a", "mine.rs", false)]);
        app.repo_root = dir.path().to_path_buf();
        // lease::release is idempotent, so with no lock file on disk it takes
        // its Ok path — the branch a lease of your own goes down.
        app.handle_release_key();
        assert_eq!(app.confirm_release, None, "no second press for your own");
        assert_eq!(app.status.as_deref(), Some("released mine.rs"));
    }

    #[test]
    fn refreshing_doctor_stores_the_report_and_resets_the_clock() {
        // Scratch dir, not the CWD: an empty repo_root makes doctor::checks read
        // whatever `.pact/` the test happens to be run from, which flips this
        // assertion depending on where you ran cargo test.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(Some("agent-a"), vec![]);
        app.repo_root = dir.path().to_path_buf();
        app.last_refresh = Instant::now() - Duration::from_secs(60);

        app.refresh_doctor();
        assert!(!app.doctor_report.as_ref().unwrap().healthy);
        assert!(app.last_refresh.elapsed() < Duration::from_secs(1));
    }

    fn render_to_string(terminal: &Terminal<ratatui::backend::TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
}
