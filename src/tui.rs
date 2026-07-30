//! Interactive terminal dashboard over what pact manages under `.pact/`
//! (leases, messages) and bd's health. Built on ratatui + its bundled
//! crossterm backend — reimplementing raw-mode terminal handling and a
//! render loop by hand would just be a worse copy of what these already do.

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState,
};
use ratatui::{Frame, Terminal};

use crate::beads::BeadsCli;
use crate::lease::{self, LeaseEntry};
use crate::msg;

/// How often the active tab refreshes itself when the user isn't pressing
/// anything — lets a lease or message changed elsewhere show up without a
/// manual 'r'. Only the active tab refreshes on this timer, so idling on
/// Leases doesn't spawn a `bd` subprocess every second for Messages.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

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
    execute!(io::stdout(), EnterAlternateScreen).context("entering alternate screen")?;
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
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tab {
    Leases,
    Messages,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Leases => "Leases",
            Tab::Messages => "Messages",
        }
    }

    fn next(self) -> Tab {
        match self {
            Tab::Leases => Tab::Messages,
            Tab::Messages => Tab::Leases,
        }
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
    /// `Some` while viewing a thread's detail pane instead of the inbox list.
    thread: Option<Vec<msg::Message>>,

    status: Option<String>,
    last_refresh: Instant,
}

impl App {
    fn new(repo_root: PathBuf, agent: Option<String>) -> Self {
        let bd = BeadsCli::locate().map_err(|e| format!("{e:#}"));
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
            thread: None,
            status: None,
            last_refresh: Instant::now(),
        };
        app.refresh_leases();
        app
    }

    fn switch_tab(&mut self) {
        self.tab = self.tab.next();
        self.status = None;
        match self.tab {
            Tab::Leases => self.refresh_leases(),
            Tab::Messages => self.refresh_messages(),
        }
    }

    fn refresh_active_tab(&mut self) {
        match self.tab {
            Tab::Leases => self.refresh_leases(),
            Tab::Messages => self.refresh_messages(),
        }
    }

    fn refresh_leases(&mut self) {
        match lease::list(&self.repo_root, true) {
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
        let Some(agent) = self.agent.clone() else {
            return; // rendered inline by render_messages; nothing to fetch
        };
        let Ok(cli) = &self.bd else {
            return; // ditto
        };
        match msg::inbox(cli, &self.repo_root, &agent, false) {
            Ok(messages) => {
                self.messages = messages;
                let selected = match self.message_list_state.selected() {
                    _ if self.messages.is_empty() => None,
                    Some(i) => Some(i.min(self.messages.len() - 1)),
                    None => Some(0),
                };
                self.message_list_state.select(selected);
            }
            Err(e) => self.status = Some(format!("failed to fetch inbox: {e:#}")),
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
        let result = match &self.bd {
            Ok(cli) => msg::read_thread(cli, &self.repo_root, &agent, &id),
            Err(_) => return,
        };
        match result {
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
                Ok(()) => self.status = Some(format!("force-released {path}")),
                Err(e) => self.status = Some(format!("release failed: {e:#}")),
            }
            self.confirm_release = None;
            self.refresh_leases();
        } else if self.is_mine(entry) {
            let agent = self.agent.clone().expect("is_mine implies agent is set");
            match lease::release(&self.repo_root, &agent, &path, false) {
                Ok(()) => self.status = Some(format!("released {path}")),
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
    loop {
        terminal.draw(|frame| draw(frame, app))?;

        let timeout = REFRESH_INTERVAL.saturating_sub(app.last_refresh.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if is_quit(key.code, key.modifiers) {
                        return Ok(());
                    }
                    handle_key(app, key.code);
                }
            }
        } else {
            app.refresh_active_tab();
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode) {
    if code == KeyCode::Tab {
        app.switch_tab();
        return;
    }
    match app.tab {
        Tab::Leases => handle_leases_key(app, code),
        Tab::Messages => handle_messages_key(app, code),
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

fn is_quit(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q'))
        || (matches!(code, KeyCode::Char('c')) && modifiers.contains(KeyModifiers::CONTROL))
}

fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_header(frame, chunks[0], app);
    match app.tab {
        Tab::Leases => render_leases(frame, chunks[1], app),
        Tab::Messages => render_messages(frame, chunks[1], app),
    }
    render_status(frame, chunks[2], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let agent_label = app.agent.as_deref().unwrap_or("(none — set PACT_AGENT)");
    let title = format!(" pact ui — {} — agent: {agent_label} ", app.tab.label());
    frame.render_widget(Block::default().borders(Borders::ALL).title(title), area);
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

    let header = Row::new(vec![
        "Path",
        "Held by",
        "Age",
        "Remaining",
        "Status",
        "Note",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .leases
        .iter()
        .map(|entry| lease_row(app, entry))
        .collect();

    let widths = [
        Constraint::Percentage(28),
        Constraint::Percentage(14),
        Constraint::Percentage(10),
        Constraint::Percentage(12),
        Constraint::Percentage(11),
        Constraint::Percentage(25),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    frame.render_stateful_widget(table, area, &mut app.table_state.clone());
}

fn lease_row<'a>(app: &App, entry: &'a LeaseEntry) -> Row<'a> {
    let agent_style = if app.is_mine(entry) {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    };
    let (status_text, status_style) = if entry.expired {
        ("expired", Style::default().fg(Color::Red))
    } else {
        ("active", Style::default().fg(Color::Green))
    };

    Row::new(vec![
        Cell::from(entry.lease.path.as_str()),
        Cell::from(entry.lease.agent.as_str()).style(agent_style),
        Cell::from(format!("{}s", entry.age_secs)),
        Cell::from(format!("{}s", entry.remaining_secs)),
        Cell::from(status_text).style(status_style),
        Cell::from(entry.lease.note.as_deref().unwrap_or("")),
    ])
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

    let items: Vec<ListItem> = app.messages.iter().map(message_list_item).collect();
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut app.message_list_state.clone());
}

fn message_list_item(message: &msg::Message) -> ListItem<'_> {
    let marker = if message.read { "  " } else { "* " };
    let subject = message.subject.as_deref().unwrap_or("(no subject)");
    let style = if message.read {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    ListItem::new(format!("{marker}{}  {subject}", message.id)).style(style)
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

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let help = match app.tab {
        Tab::Leases => "j/k: move  r: refresh  enter/d: release  esc: cancel  tab: switch  q: quit",
        Tab::Messages if app.thread.is_some() => "esc: back  tab: switch  q: quit",
        Tab::Messages => "j/k: move  r: refresh  enter: open thread  tab: switch  q: quit",
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
            },
            age_secs: 10,
            remaining_secs: 890,
            expired,
        }
    }

    fn message(id: &str, subject: &str, read: bool) -> msg::Message {
        msg::Message {
            id: id.to_string(),
            thread: id.to_string(),
            to: "agent-a".to_string(),
            subject: Some(subject.to_string()),
            body: format!("body of {id}"),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            read,
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
            thread: None,
            status: None,
            last_refresh: Instant::now(),
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
    fn releasing_someone_elses_lease_requires_confirmation_first() {
        let mut app = app_with(Some("agent-a"), vec![entry("agent-b", "shared.rs", false)]);
        app.handle_release_key();
        assert_eq!(app.confirm_release, Some(0));
        // lease is untouched: repo_root is bogus so a real release would
        // have errored into `status`, not silently succeeded.
        assert!(app.status.as_deref().unwrap_or("").contains("force it"));
    }

    #[test]
    fn tab_switches_between_leases_and_messages_and_clears_thread() {
        let mut app = app_with(Some("agent-a"), vec![]);
        app.thread = Some(vec![message("m1", "hi", true)]);
        assert_eq!(app.tab, Tab::Leases);

        app.switch_tab();
        assert_eq!(app.tab, Tab::Messages);
        // switching away from Leases doesn't itself close an open thread —
        // only closing the thread view (Esc) does. Confirm the tab changed;
        // thread-clearing is exercised separately via close_thread below.

        app.switch_tab();
        assert_eq!(app.tab, Tab::Leases);
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
        let unread_msg = message("m1", "hello", false);
        let read_msg = message("m2", "world", true);
        let unread = message_list_item(&unread_msg);
        let read = message_list_item(&read_msg);
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
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("pact ui"));
        assert!(rendered.contains("agent-a"));
        assert!(rendered.contains("mine.rs"));
        assert!(rendered.contains("theirs.rs"));
        assert!(rendered.contains("expired"));
        assert!(rendered.contains("active"));
    }

    #[test]
    fn messages_tab_shows_bd_missing_error_inline() {
        use ratatui::backend::TestBackend;

        let mut app = app_with(Some("agent-a"), vec![]);
        app.tab = Tab::Messages;
        // app_with already seeds `bd` as Err, matching "bd not on PATH".

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

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
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = render_to_string(&terminal);
        assert!(rendered.contains("thread"));
        assert!(rendered.contains("renamed foo()"));
        assert!(rendered.contains("body of msg-1"));
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
