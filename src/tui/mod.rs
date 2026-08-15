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
//! register itself. Each screen owns its own `State`, which `App` holds as one
//! field, so a screen that needs a new piece of state adds it to its own file.

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
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use nav::{Nav, Screen, View};

/// How often the current view refreshes itself when the user isn't pressing
/// anything — lets a lease or message changed elsewhere show up without a
/// manual 'r'. Only the current view refreshes on this timer, so sitting on
/// Fleet doesn't spawn a `bd` subprocess every second for Messages.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// How often the Messages unread badge is refreshed from a screen that isn't
/// Messages. The badge has to be current from ANY screen (an arriving message
/// is invisible otherwise), but the whole point of the rule above is that
/// idling on Fleet doesn't spawn `bd` every second — so the badge gets its own,
/// deliberately slower clock. It is checked on the 1 s wake that already
/// happens, so it adds no event-loop wakeups and never touches the animation
/// clock. On Messages it costs nothing at all: the inbox fetch there recomputes
/// the count for free.
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

/// The keys that mean the same thing on every screen, and therefore belong in
/// the help line after whatever the current screen adds.
const GLOBAL_HELP: &str = "enter: open  esc: back  tab: screen  1-4: jump  r: refresh  q: quit";

/// Hand a call to whichever module owns the current screen.
///
/// Every screen module exposes the same set of functions, so this is a match
/// with no special cases — and a module that forgets one of them fails to
/// compile rather than quietly falling through to a default.
macro_rules! dispatch {
    ($screen:expr, $f:ident ( $($arg:expr),* $(,)? )) => {
        match $screen {
            Screen::Fleet => fleet::$f($($arg),*),
            Screen::Activity => activity::$f($($arg),*),
            Screen::Messages => messages::$f($($arg),*),
            Screen::Detail => detail::$f($($arg),*),
            Screen::Health => health::$f($($arg),*),
        }
    };
}

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

struct App {
    repo_root: PathBuf,
    /// The resolved pact identity, if any. `None` means no `--agent`/
    /// `PACT_AGENT` was set — every lease then looks like someone else's, and
    /// there is no inbox to show (whose would it be?). The operator is not
    /// necessarily a fleet member.
    agent: Option<String>,

    /// Where we are, and how we got here. Replaces the old `tab` enum and the
    /// `thread: Option<..>` that was a one-level stack for the one drill-in
    /// that existed.
    nav: Nav,

    /// Every store under `.pact/` this dashboard reads, parsed once per tick.
    /// Views project from here rather than re-reading a file each.
    data: data::Store,

    // One per screen, each owned by its own module.
    fleet: fleet::State,
    messages: messages::State,
    health: health::State,
    // Empty until the beads that own those screens fill them in; the field
    // exists now so neither has to edit this file to add one.
    #[allow(dead_code)]
    activity: activity::State,
    #[allow(dead_code)]
    detail: detail::State,

    status: Option<String>,
    last_refresh: Instant,

    /// Where the tab bar (its inner, post-border area) and the current view's
    /// content were last drawn — recorded each frame so mouse clicks and hover
    /// both read the exact same rects rendering used, rather than a second,
    /// possibly-drifted approximation of the layout.
    header_area: Rect,
    content_area: Rect,
    /// Root tab currently under the mouse cursor, as an index into
    /// [`View::roots`].
    hovered_tab: Option<usize>,
    /// Row currently under the mouse cursor in the current view's list.
    hovered_row: Option<usize>,
}

impl App {
    fn new(repo_root: PathBuf, agent: Option<String>) -> Self {
        let mut app = App {
            repo_root,
            agent,
            nav: Nav::default(),
            data: data::Store::default(),
            fleet: fleet::State::default(),
            activity: activity::State::default(),
            messages: messages::State::default(),
            detail: detail::State::default(),
            health: health::State::default(),
            status: None,
            last_refresh: Instant::now(),
            header_area: Rect::default(),
            content_area: Rect::default(),
            hovered_tab: None,
            hovered_row: None,
        };
        app.refresh_current_view();
        app
    }

    /// Re-read everything the current view needs. The only place the read model
    /// is refreshed, and the only place `last_refresh` moves: this is the clock
    /// the event loop's poll timeout is derived from.
    fn refresh_current_view(&mut self) {
        // Once per tick, before any view asks for anything: the whole point of
        // the read model is that N views cost one parse, not N.
        self.data.refresh(&self.repo_root);
        let screen = self.nav.current().screen();
        dispatch!(screen, refresh(self));
        messages::refresh_unread_if_due(self);
        // Same reasoning, second badge: the Health indicator has to be current
        // from ANY screen — an operator who never opens Health would otherwise
        // never learn a setup check is failing — and doctor spawns `bd`, so it
        // has a clock of its own (`health::SETUP_INTERVAL`) rather than this
        // one.
        health::refresh_setup_if_due(self);
        self.last_refresh = Instant::now();
    }

    /// Enter: drill in. Never mutates — `on_enter` takes `&App`, so that is a
    /// compile error rather than a code review.
    fn open_selected(&mut self) {
        let screen = self.nav.current().screen();
        if let Some(view) = dispatch!(screen, on_enter(self)) {
            self.nav.push(view);
            self.status = None;
            self.hovered_row = None;
            self.refresh_current_view();
        }
    }

    /// Esc: go back. One meaning, on every screen — it used to mean "cancel a
    /// release" on Leases and "close the thread" on Messages.
    ///
    /// At a root there is no view to pop, and that is the one place it does
    /// anything else: it clears an armed force-release and the status line,
    /// which is the transient state a user pressing Esc at the top is asking to
    /// be rid of.
    fn back(&mut self) {
        if self.nav.pop() {
            self.status = None;
            self.hovered_row = None;
            self.refresh_current_view();
        } else if !fleet::cancel_confirm(self) {
            self.status = None;
        }
    }

    /// Switch the question being asked. Roots replace the stack; drill-ins
    /// belong to the root they were opened from, so they go with it.
    fn jump_root(&mut self, index: usize) {
        let roots = View::roots();
        let Some(root) = roots.get(index) else {
            return;
        };
        if self.nav.root() == root && self.nav.depth() == 1 {
            return;
        }
        self.nav.set_root(root.clone());
        self.status = None;
        // Stale from the previous screen's list — cleared here rather than left
        // to the next mouse-move event, so a keyboard-driven switch never shows
        // a leftover highlight on an unrelated row.
        self.hovered_row = None;
        self.refresh_current_view();
    }

    fn cycle_root(&mut self, delta: isize) {
        let next = self.nav.cycle_root(delta);
        let index = View::roots().iter().position(|r| r == &next).unwrap_or(0);
        self.jump_root(index);
    }
}

/// The root tabs as they are actually rendered, unread badge included.
///
/// Everything that measures a tab goes through this, so widening "Messages" to
/// "Messages (3)" widens its rect too — a badge that only rendering knew about
/// would shift every later tab out from under its own hit-box.
fn root_labels(app: &App) -> Vec<String> {
    View::roots()
        .iter()
        .map(|root| match root {
            // The count alone is easy to miss while you're reading another
            // screen, which is the exact situation this badge exists for.
            View::Messages if app.messages.unread > 0 => {
                format!("Messages ({})", app.messages.unread)
            }
            // The other badge: `Health !` / `Health ✗`. Demoting 25 setup
            // checks to a collapsed line must not lose the one job the tab was
            // doing — making a failing check impossible to miss from anywhere.
            View::Health => health::tab_label(app),
            other => other.label(),
        })
        .collect()
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
        // loop may ever wake on: refresh_current_view() shells out to
        // `bd`/doctor and re-parses the whole event and message store, so a
        // shorter timeout here is ~10 subprocesses and ~10 full re-parses a
        // second. A zero remaining wakes immediately, refreshes below, and
        // resets the clock — so it cannot spin either.
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
            app.refresh_current_view();
        }
    }
}

/// The keys that mean the same thing everywhere are handled here; everything
/// else belongs to the current view, movement included, because movement is
/// selection state and selection state belongs to the list that owns it.
fn handle_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Tab => return app.cycle_root(1),
        KeyCode::BackTab => return app.cycle_root(-1),
        KeyCode::Char(c @ '1'..='4') => {
            return app.jump_root(c as usize - '1' as usize);
        }
        KeyCode::Enter => return app.open_selected(),
        KeyCode::Esc => return app.back(),
        KeyCode::Char('r') => return app.refresh_current_view(),
        _ => {}
    }
    let screen = app.nav.current().screen();
    let _ = dispatch!(screen, handle_key(app, code));
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
    if let Some(index) = widgets::tab_at(app.header_area, &root_labels(app), x, y) {
        app.jump_root(index);
        return;
    }
    if !widgets::rect_contains(app.content_area, x, y) {
        return;
    }
    // A click SELECTS. Opening is Enter's job, everywhere — the one key that
    // used to mutate on click-adjacent paths is exactly the defect this spine
    // was written for.
    let screen = app.nav.current().screen();
    if let Some(index) = dispatch!(screen, row_at(app, x, y)) {
        dispatch!(screen, select(app, index));
    }
}

/// A wheel event is the same movement j/k is, so it goes through the same key
/// handler rather than a parallel path the two could drift apart on.
fn handle_scroll(app: &mut App, delta: isize) {
    let code = if delta > 0 {
        KeyCode::Down
    } else {
        KeyCode::Up
    };
    let screen = app.nav.current().screen();
    let _ = dispatch!(screen, handle_key(app, code));
}

/// Updates hover state so the header/list can highlight whatever's directly
/// under the cursor, before a click commits to anything. Requested after a user
/// found the tab bar unresponsive on their terminal: with nothing to show which
/// zone a click would land in, a slight hit-test mismatch (the old equal-thirds
/// approximation vs. the tabs' real, unequal widths) just looked like "mouse
/// doesn't work" rather than "clicked the wrong spot".
///
/// Hover and click ask the same `row_at`, so they cannot disagree about which
/// row is under the cursor.
fn update_hover(app: &mut App, x: u16, y: u16) {
    app.hovered_tab = widgets::tab_at(app.header_area, &root_labels(app), x, y);

    app.hovered_row = if widgets::rect_contains(app.content_area, x, y) {
        let screen = app.nav.current().screen();
        dispatch!(screen, row_at(app, x, y))
    } else {
        None
    };
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

    // Recorded so row_at/rect_contains hit-test against the exact rect the view
    // was rendered into — the same discipline tab_rects follows for the header,
    // and the reason a click can never land on a row it did not hit.
    let content = chunks[1];
    app.content_area = content;

    render_header(frame, chunks[0], app);
    let screen = app.nav.current().screen();
    dispatch!(screen, render(frame, content, app));
    render_status(frame, chunks[2], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &mut App) {
    let agent_label = app.agent.as_deref().unwrap_or("(none — set PACT_AGENT)");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" pact ui — agent: {agent_label} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Recorded so hit-testing (tab_at) and hover (update_hover) use the exact
    // same area rendering just used — see widgets::tab_rects.
    app.header_area = inner;

    let labels = root_labels(app);
    let rects = widgets::tab_rects(inner, &labels);
    let active = app.nav.root_index();

    for (index, rect) in rects.iter().enumerate() {
        let selected = index == active;
        let hovered = !selected && app.hovered_tab == Some(index);
        let style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if hovered {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else if View::roots()[index] == View::Messages && app.messages.unread > 0 {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(format!(" {} ", labels[index])).style(style),
            *rect,
        );
    }

    // The stack, to the right of the tabs. An operator three levels deep needs
    // to see where they are and what Esc takes them back to; a root's crumb is
    // just its own name, which costs nothing to leave showing.
    if let Some(last) = rects.last() {
        let x = last.x + last.width + 2;
        if x < inner.x + inner.width {
            let rect = Rect::new(x, inner.y, inner.x + inner.width - x, 1);
            frame.render_widget(
                Paragraph::new(app.nav.breadcrumb()).style(Style::default().fg(Color::DarkGray)),
                rect,
            );
        }
    }
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let screen = app.nav.current().screen();
    let help = format!("{}  {GLOBAL_HELP}", dispatch!(screen, help()));
    let line = match &app.status {
        Some(status) => format!("{status}   ({help})"),
        None => help,
    };
    let style = if fleet::awaiting_confirmation(app) {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(Paragraph::new(Line::from(line)).style(style), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor;
    use crate::lease::{self, LeaseEntry};
    use crate::msg;
    use ratatui::backend::TestBackend;

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

    /// An App with nothing on disk behind it. `repo_root` is empty, so any
    /// path that would touch a real `.pact/` errors into `status` instead of
    /// silently succeeding — which several tests below rely on.
    fn app_with(agent: Option<&str>, leases: Vec<LeaseEntry>) -> App {
        let mut app = App {
            repo_root: PathBuf::new(),
            agent: agent.map(str::to_string),
            nav: Nav::default(),
            data: data::Store::default(),
            fleet: fleet::State::default(),
            activity: activity::State::default(),
            messages: messages::State::default(),
            detail: detail::State::default(),
            health: health::State::default(),
            status: None,
            last_refresh: Instant::now(),
            header_area: Rect::new(0, 0, 90, 1),
            content_area: Rect::new(0, 3, 80, 8),
            hovered_tab: None,
            hovered_row: None,
        };
        app.fleet.leases = leases;
        app.fleet.table.select(Some(0));
        app.fleet.selected = app.fleet.leases.first().map(|e| e.lease.path.clone());
        app.messages.list.select(Some(0));
        app.messages.last_unread_refresh = Instant::now();
        app
    }

    fn render_to_string(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn quits_on_q_or_ctrl_c_only() {
        assert!(is_quit(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(is_quit(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!is_quit(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(!is_quit(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!is_quit(KeyCode::Enter, KeyModifiers::NONE));
    }

    /// The spine, through the key handler an operator actually presses.
    #[test]
    fn enter_drills_in_and_esc_comes_back_out() {
        let mut app = app_with(
            Some("agent-a"),
            vec![entry("agent-b", "src/lease.rs", false)],
        );
        assert_eq!(app.nav.current(), &View::Fleet);

        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(app.nav.current(), &View::Path("src/lease.rs".into()));

        handle_key(&mut app, KeyCode::Esc);
        assert_eq!(app.nav.current(), &View::Fleet);
        // And Esc at the root does not empty the stack out from under us.
        handle_key(&mut app, KeyCode::Esc);
        assert_eq!(app.nav.current(), &View::Fleet);
    }

    /// The acceptance criterion, and the reason release moved to `x`: Enter is
    /// the universal look-closer key, and it used to force-release.
    #[test]
    fn enter_never_releases_a_lease() {
        let mut app = app_with(Some("agent-a"), vec![entry("agent-b", "shared.rs", false)]);
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.fleet.confirm_release, None,
            "Enter must not arm a release"
        );
        assert_eq!(app.status, None, "Enter must not report a mutation");
        assert_eq!(app.nav.current(), &View::Path("shared.rs".into()));
    }

    #[test]
    fn releasing_someone_elses_lease_still_takes_two_presses() {
        let mut app = app_with(Some("agent-a"), vec![entry("agent-b", "shared.rs", false)]);
        handle_key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.fleet.confirm_release.as_deref(), Some("shared.rs"));
        // The lease is untouched: repo_root is bogus, so a real release would
        // have errored into `status` rather than silently succeeding.
        assert!(app.status.as_deref().unwrap_or("").contains("force it"));

        // Esc at the root disarms it — the one thing Esc does when there is no
        // view to pop.
        handle_key(&mut app, KeyCode::Esc);
        assert_eq!(app.fleet.confirm_release, None);
        assert_eq!(app.status, None, "the prompt goes with the armed state");
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
        handle_key(&mut app, KeyCode::Char('x'));
        assert_eq!(
            app.fleet.confirm_release, None,
            "no second press for your own"
        );
        assert_eq!(app.status.as_deref(), Some("released mine.rs"));
    }

    /// pact-pyt.1's other acceptance criterion: a lease released by ANOTHER
    /// agent during the 1 s refresh must not slide the operator's cursor onto a
    /// different row — with a release key one press away.
    #[test]
    fn a_release_elsewhere_does_not_move_the_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(Some("operator"), vec![]);
        app.repo_root = dir.path().to_path_buf();
        app.fleet.leases = vec![
            entry("a", "a.rs", false),
            entry("b", "b.rs", false),
            entry("c", "c.rs", false),
        ];
        fleet::select(&mut app, 2);
        assert_eq!(app.fleet.selected.as_deref(), Some("c.rs"));

        // b.rs is released by its holder somewhere else; the list shortens and
        // every row below it moves up one.
        app.fleet.leases.remove(1);
        let keys = ["a.rs", "c.rs"];
        let index = widgets::reselect(
            &keys,
            app.fleet.selected.as_deref(),
            app.fleet.table.selected(),
        );
        fleet::select(&mut app, index.unwrap());

        assert_eq!(app.fleet.selected.as_deref(), Some("c.rs"), "same lease");
        assert_eq!(app.fleet.table.selected(), Some(1), "different row");
    }

    #[test]
    fn roots_cycle_with_tab_and_jump_with_digits() {
        let mut app = app_with(Some("agent-a"), vec![]);
        assert_eq!(app.nav.current(), &View::Fleet);

        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.nav.current(), &View::Activity);
        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.nav.current(), &View::Messages);
        handle_key(&mut app, KeyCode::BackTab);
        assert_eq!(app.nav.current(), &View::Activity);

        handle_key(&mut app, KeyCode::Char('4'));
        assert_eq!(app.nav.current(), &View::Health);
        handle_key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.nav.current(), &View::Fleet);
    }

    /// Roots answer different questions, so switching one replaces the stack
    /// rather than burying a drill-in under it.
    #[test]
    fn switching_root_drops_the_drill_in_it_was_opened_from() {
        let mut app = app_with(Some("agent-a"), vec![entry("a", "one.rs", false)]);
        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(app.nav.depth(), 2);
        handle_key(&mut app, KeyCode::Char('4'));
        assert_eq!(app.nav.current(), &View::Health);
        assert_eq!(app.nav.depth(), 1);
    }

    #[test]
    fn clicking_a_lease_row_selects_it_but_does_not_open_it() {
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
        handle_click(&mut app, 0, 4);
        assert_eq!(app.fleet.table.selected(), Some(0));
        handle_click(&mut app, 0, 6);
        assert_eq!(app.fleet.table.selected(), Some(2));
        assert_eq!(app.fleet.selected.as_deref(), Some("three.rs"));
        // A click never navigates — Enter does that, everywhere.
        assert_eq!(app.nav.current(), &View::Fleet);
        // Out-of-range click is a no-op, not a panic or a bogus selection.
        handle_click(&mut app, 0, 50);
        assert_eq!(app.fleet.table.selected(), Some(2));
    }

    #[test]
    fn clicking_the_header_switches_root() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.header_area = Rect::new(0, 0, 90, 1);
        app.content_area = Rect::new(0, 1, 90, 8);

        let health_rect = widgets::tab_rects(app.header_area, &root_labels(&app))[3];
        handle_click(&mut app, health_rect.x, health_rect.y);
        assert_eq!(app.nav.current(), &View::Health);

        // Same click path with an unread badge widening the Messages label:
        // handle_click reads the same labels rendering did, so the rects it
        // tests against are the ones that were drawn.
        app.messages.unread = 3;
        let shifted = widgets::tab_rects(app.header_area, &root_labels(&app))[3];
        assert_ne!(shifted.x, health_rect.x);
        app.jump_root(0);
        handle_click(&mut app, shifted.x, shifted.y);
        assert_eq!(app.nav.current(), &View::Health);
    }

    #[test]
    fn hovering_a_tab_sets_hovered_tab_without_switching() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.header_area = Rect::new(0, 0, 90, 1);
        app.content_area = Rect::new(0, 1, 90, 8);

        let messages_rect = widgets::tab_rects(app.header_area, &root_labels(&app))[2];
        update_hover(&mut app, messages_rect.x, messages_rect.y);
        assert_eq!(app.hovered_tab, Some(2));
        assert_eq!(app.nav.current(), &View::Fleet); // hover alone never switches

        update_hover(&mut app, 0, 50); // moving off the header clears it
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

        // switching root clears a stale hover from the previous list
        app.cycle_root(1);
        assert_eq!(app.hovered_row, None);
    }

    #[test]
    fn scrolling_moves_the_current_views_selection() {
        let mut app = app_with(
            Some("agent-a"),
            vec![
                entry("agent-a", "one.rs", false),
                entry("agent-a", "two.rs", false),
            ],
        );
        handle_scroll(&mut app, 1);
        assert_eq!(app.fleet.table.selected(), Some(1));
        handle_scroll(&mut app, -1);
        assert_eq!(app.fleet.table.selected(), Some(0));
    }

    /// The ui exports on a timer, and the timer has to be readable off the two
    /// clocks that already exist. Too short and every frame carries a POST;
    /// longer than a coffee break and `pact ui` is back to what pact-aw7.9
    /// found — one batch at exit, nothing at all on Ctrl-C, and a whole
    /// session's gauges collapsed onto the exit timestamp.
    #[test]
    fn telemetry_is_exported_on_a_clock_the_event_loop_already_wakes_for() {
        assert!(EXPORT_INTERVAL >= REFRESH_INTERVAL);
        assert!(EXPORT_INTERVAL <= Duration::from_secs(60));
    }

    #[test]
    fn the_unread_badge_refreshes_on_its_own_slower_clock() {
        // The badge must not turn Fleet into a once-a-second `bd` spawner, so
        // it has a clock of its own that gates the inbox fetch.
        assert!(UNREAD_INTERVAL >= REFRESH_INTERVAL * 5);

        let mut app = app_with(Some("agent-a"), vec![]);
        let long_ago = Instant::now() - Duration::from_secs(60);

        app.messages.last_unread_refresh = Instant::now();
        messages::refresh_unread_if_due(&mut app);
        assert!(
            app.messages.last_unread_refresh.elapsed() < Duration::from_secs(1),
            "not due yet: the inbox must not be fetched"
        );

        app.messages.last_unread_refresh = long_ago;
        messages::refresh_unread_if_due(&mut app);
        assert!(app.messages.last_unread_refresh.elapsed() < Duration::from_secs(1));

        // Armed for a force-release: the fetch would overwrite the confirmation
        // prompt in the status line, so it waits.
        app.messages.last_unread_refresh = long_ago;
        app.fleet.confirm_release = Some("shared.rs".to_string());
        messages::refresh_unread_if_due(&mut app);
        assert!(app.messages.last_unread_refresh.elapsed() >= Duration::from_secs(60));
    }

    #[test]
    fn header_renders_the_unread_count_from_another_screen() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.messages.unread = 3;

        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(render_to_string(&terminal).contains("Messages (3)"));

        app.messages.unread = 0;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("Messages"));
        assert!(!rendered.contains("Messages ("));
    }

    #[test]
    fn the_header_shows_the_stack_as_a_breadcrumb() {
        let mut app = app_with(Some("agent-a"), vec![entry("a", "src/lease.rs", false)]);
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();

        handle_key(&mut app, KeyCode::Enter);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(render_to_string(&terminal).contains("Fleet > src/lease.rs"));
    }

    #[test]
    fn the_status_line_leads_with_the_release_key_on_fleet() {
        let mut app = app_with(Some("agent-a"), vec![]);
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("x: release"), "{rendered}");
        assert!(rendered.contains("esc: back"), "{rendered}");
    }

    #[test]
    fn renders_the_leases_table_without_panicking() {
        let mut app = app_with(
            Some("agent-a"),
            vec![
                entry("agent-a", "mine.rs", false),
                entry("agent-b", "theirs.rs", true),
            ],
        );

        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
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
    /// where a force-release gets decided, and it used to render the
    /// misreadable `80s  3520s  active`.
    #[test]
    fn leases_table_shows_age_and_state_never_a_raw_countdown() {
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
    /// Inverted deliberately. This used to assert that a missing `bd` REPLACED
    /// the message pane with "bd (beads) not found" — which was true of the
    /// dashboard and false of pact: messages moved into
    /// `.pact/messages.jsonl` in 0.9.0 and exit 3 was retired with them, so the
    /// whole `msg` surface works with no `bd` on PATH. The screen refused to
    /// draw the fleet's conversation in exactly the situation where an operator
    /// most needs to read it — a fresh clone with no tooling installed.
    ///
    /// The old assertion is what kept the gate there, so it had to go with it.
    fn a_missing_bd_does_not_hide_the_conversation() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.nav.set_root(View::Messages);
        // app_with seeds `bd` as Err, matching "bd not on PATH".

        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("Messages"));
        assert!(
            !rendered.contains("bd (beads) not found"),
            "the message store does not need bd: {rendered}"
        );
    }

    /// A REAL store, not a seeded cache. Enter pushes `View::Thread`, and the
    /// refresh that follows re-fetches the thread from `repo_root` — so a
    /// fixture assigned straight to `app.messages.thread` is overwritten with
    /// the empty result of reading an empty path before it can ever be drawn.
    /// The seeded version of this test asserted against that empty pane and
    /// failed on the `thread` body while passing on the block title, which
    /// reads like a rendering bug and is not one.
    #[test]
    fn a_message_opens_into_its_thread_and_esc_returns_to_the_list() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let root = tmp.path();
        let sent = msg::send(
            root,
            "agent-b",
            &["agent-a".to_string()],
            msg::Draft {
                thread: None,
                subject: Some("renamed foo()"),
                body: "the signature lost its second parameter",
                about: &[],
                notice: false,
            },
        )
        .unwrap();
        let id = sent[0].id.clone();

        let mut app = app_with(Some("agent-a"), vec![]);
        app.repo_root = root.to_path_buf();
        app.nav.set_root(View::Messages);
        app.refresh_current_view();
        app.messages.list.select(Some(0));

        handle_key(&mut app, KeyCode::Enter);
        assert_eq!(app.nav.current(), &View::Thread(id.clone()));

        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("thread"), "{rendered}");
        assert!(rendered.contains("renamed foo()"), "{rendered}");
        assert!(
            rendered.contains("the signature lost its second parameter"),
            "{rendered}"
        );

        handle_key(&mut app, KeyCode::Esc);
        assert_eq!(app.nav.current(), &View::Messages);
    }

    #[test]
    fn health_prompts_before_its_first_run_then_shows_checks() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.nav.set_root(View::Health);

        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(render_to_string(&terminal).contains("press r"));

        app.health.report = Some(doctor::DoctorReport {
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

    #[test]
    fn refreshing_health_stores_the_report_and_resets_the_clock() {
        // Scratch dir, not the CWD: an empty repo_root makes doctor::checks read
        // whatever `.pact/` the test happens to be run from, which flips this
        // assertion depending on where you ran cargo test.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(Some("agent-a"), vec![]);
        app.repo_root = dir.path().to_path_buf();
        app.nav.set_root(View::Health);
        app.last_refresh = Instant::now() - Duration::from_secs(60);

        handle_key(&mut app, KeyCode::Char('r'));
        assert!(!app.health.report.as_ref().unwrap().healthy);
        assert!(app.last_refresh.elapsed() < Duration::from_secs(1));
    }

    /// The view owns the whole content chunk at every size, and clicks land on
    /// the row they hit. The invariant: content_area IS the rect the view
    /// rendered into.
    #[test]
    fn the_view_owns_the_content_chunk_and_clicks_map_to_rows_at_any_size() {
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
            let top = app.content_area.y;
            handle_click(&mut app, 0, top);
            assert_eq!(app.fleet.table.selected(), Some(0)); // column header: no change
            handle_click(&mut app, 0, top + 3);
            assert_eq!(app.fleet.table.selected(), Some(2), "at {cols}x{rows}");
            fleet::select(&mut app, 0);
        }
    }

    /// Every screen renders, and every screen's contract is dispatchable — a
    /// stub that forgot a function would not compile, but a screen that panics
    /// on an empty store still would.
    #[test]
    fn every_root_renders_on_an_empty_store() {
        let mut app = app_with(None, vec![]);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        for index in 0..View::roots().len() {
            app.jump_root(index);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        }
    }
}
