//! Activity: the live event feed, so an idle fleet and a finished one stop
//! looking alike.
//!
//! The dashboard used to render pact's STATE and never its CHANGE, so
//! "nothing is happening" and "everything finished" looked identical and the
//! operator's answer was to run `pact log` in a second terminal — the command
//! AGENTS.md tells agents to orient with, absent from the human's own
//! dashboard. This is that feed.
//!
//! Three things it does that a table of the same rows would not:
//!
//! - **Newest last, following the tail only while the selection is on it.** A
//!   log reads top to bottom (same as `render_log` in main.rs). Auto-scroll
//!   that fights an operator who has scrolled up is worse than none, so the
//!   follow is the selection: on the last row means following, anywhere else
//!   means reading history.
//! - **Colour by kind.** The vocabulary is small and the meanings are not
//!   equal: `acquired`/`released` are traffic, `refused`/`stolen`/`expired`/
//!   `force-released` are contention, `notified`/`annotation` are the
//!   subscription machinery working.
//! - **Narrowed by what is selected elsewhere.** An agent or a path selected
//!   on Fleet narrows this feed to it. That contextual narrowing IS the
//!   navigation; `c` widens back to the whole fleet.
//!
//! Ages and detail text come from main.rs's own `since` / `one_line`, not from
//! a second formatter here — if this feed and `pact log` disagreed about how
//! old something is, one of them would be lying.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use ratatui::crossterm::event::KeyCode;

use super::nav::View;
use super::widgets;
use super::App;
use crate::agents::AgentInfo;
use crate::events::Event;
use crate::lease::human_secs;

/// How many events the feed holds. The log is bounded (rewritten to its newest
/// 4000 lines past 5000), so this is a rendering bound rather than a read
/// bound: `Store` has already parsed the file either way, and a feed nobody
/// can scroll to the bottom of in one session is not more useful for being
/// longer.
const FEED_LIMIT: usize = 500;

/// The window the header rate is measured over. Five minutes is long enough
/// that a fleet between commands does not read as dead, and short enough that
/// a fleet that stopped an hour ago reads as stopped.
const RATE_WINDOW_SECS: i64 = 300;

/// What a kind means, which is what it is coloured by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The fleet working: claims taken, held and given back.
    Traffic,
    /// Two agents wanting the same path, and how that resolved.
    Contention,
    /// The subscription machinery doing its job, and corrections to the log.
    Machinery,
    /// A kind this build does not know about. Never guessed into a class —
    /// the log outlives any one binary's idea of what can be in it.
    Other,
}

impl Kind {
    fn color(self) -> Color {
        match self {
            Kind::Traffic => Color::Green,
            Kind::Contention => Color::Red,
            Kind::Machinery => Color::Blue,
            Kind::Other => Color::DarkGray,
        }
    }
}

/// The ten kinds a real log carries, by what they mean.
pub fn classify(kind: &str) -> Kind {
    match kind {
        "acquired" | "released" | "renewed" | "watched" => Kind::Traffic,
        "refused" | "stolen" | "expired" | "force-released" => Kind::Contention,
        "notified" | "annotation" => Kind::Machinery,
        _ => Kind::Other,
    }
}

/// What this feed is narrowed to, resolved from the selection the operator
/// made on another screen.
#[derive(Clone, PartialEq, Debug)]
pub enum Context {
    Agent(String),
    Path(String),
}

/// One rendered row, computed once per tick rather than once per frame.
///
/// Owned strings: ages are relative to the refresh that built them, so the
/// feed cannot show one row's age against another row's clock.
pub struct FeedRow {
    /// The event's line number in the log — its identity across a refresh.
    pub id: usize,
    pub when: String,
    pub agent: String,
    pub kind: String,
    /// The leased path, or empty for an event that is not about one.
    pub target: String,
    pub detail: String,
    pub class: Kind,
}

/// Which entity Enter opens from the selected row. Every event has an agent;
/// most, but not all, have a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Link {
    #[default]
    Path,
    Agent,
}

pub struct State {
    pub rows: Vec<FeedRow>,
    pub table: TableState,
    /// The selected event's id, kept alongside the index so a tick that slides
    /// the feed window restores the *event* rather than the row number.
    pub selected: Option<usize>,
    /// Follow the tail. True exactly while the selection is the last row —
    /// which is also how the wheel and j/k read it, so scrolling up stops the
    /// follow and coming back to the bottom resumes it.
    pub follow: bool,
    pub link: Link,
    /// Ignore the contextual narrowing and show the whole fleet. Without this
    /// the global feed is unreachable: Fleet always has something selected.
    pub all: bool,
    /// Events in the last [`RATE_WINDOW_SECS`], over the rows being shown.
    pub recent: usize,
    pub context: Option<Context>,
}

impl Default for State {
    fn default() -> Self {
        State {
            rows: Vec::new(),
            table: TableState::default(),
            selected: None,
            // A feed opened for the first time is showing you now, not 500
            // events ago.
            follow: true,
            link: Link::default(),
            all: false,
            recent: 0,
            context: None,
        }
    }
}

/// What the feed narrows to, given what is selected on Fleet.
///
/// Resolved against the roster rather than assumed: Fleet's selection is a
/// name today and a path in the build before it, and this screen has no
/// business knowing which. A selection that names a live agent narrows by
/// agent; anything else is a path.
pub fn context_of(selected: Option<&str>, roster: &[AgentInfo], all: bool) -> Option<Context> {
    if all {
        return None;
    }
    let selected = selected.filter(|s| !s.is_empty())?;
    if roster.iter().any(|a| a.name == selected) {
        Some(Context::Agent(selected.to_string()))
    } else {
        Some(Context::Path(selected.to_string()))
    }
}

/// The detail column, with the fields that carry the contention chain put
/// *first*.
///
/// `refused` carries `holder`/`holder_remaining_secs` and `notified` carries
/// `subscriber`/`message_id`; those are what let a reader follow a refusal to
/// the release that answered it. Flattening them behind a free-text note would
/// lose them to the first truncation, so the note goes last.
pub fn detail_of(e: &Event) -> String {
    let mut parts: Vec<String> = Vec::new();
    match e.kind.as_str() {
        "refused" => {
            if let Some(holder) = &e.holder {
                parts.push(match e.holder_remaining_secs {
                    Some(left) => format!("held by {holder}, {} left", human_secs(left)),
                    None => format!("held by {holder}"),
                });
            }
        }
        "notified" => {
            if let Some(subscriber) = &e.subscriber {
                parts.push(format!("to {subscriber}"));
            }
            if let Some(id) = &e.message_id {
                parts.push(id.clone());
            }
        }
        "force-released" | "stolen" => {
            if let Some(displaced) = &e.displaced {
                parts.push(format!("displaced {displaced}"));
            }
        }
        "annotation" => {
            if let Some(actor) = &e.actor {
                parts.push(format!("by {actor}"));
            }
            if let Some(lines) = &e.covers_lines {
                parts.push(format!("covers {} line(s)", lines.len()));
            }
        }
        _ => {}
    }
    if let Some(detail) = e.detail.as_deref().filter(|d| !d.is_empty()) {
        parts.push(detail.to_string());
    }
    crate::one_line(&parts.join(" · "), 60)
}

/// Rows for the feed, oldest first — the order the slices already come in.
pub fn feed_rows(events: &[&(usize, Event)]) -> Vec<FeedRow> {
    events
        .iter()
        .map(|(id, e)| FeedRow {
            id: *id,
            when: crate::since(&e.at),
            agent: e.agent.clone(),
            kind: e.kind.clone(),
            target: e.path.clone().unwrap_or_default(),
            detail: detail_of(e),
            class: classify(&e.kind),
        })
        .collect()
}

/// How many of these events landed in the last `window` seconds — the
/// one-glance answer to "is anything happening at all".
///
/// An unparsable stamp is not recent. It sorts oldest everywhere else in pact
/// for the same reason: a corrupt line must not pass itself off as news.
pub fn rate(events: &[&(usize, Event)], now: chrono::DateTime<chrono::Utc>, window: i64) -> usize {
    events
        .iter()
        .filter(|(_, e)| match chrono::DateTime::parse_from_rfc3339(&e.at) {
            Ok(at) => (now - at.with_timezone(&chrono::Utc)).num_seconds() <= window,
            Err(_) => false,
        })
        .count()
}

pub fn refresh(app: &mut App) {
    let context = context_of(
        app.fleet.selected.as_deref(),
        app.data.roster(),
        app.activity.all,
    );

    // Every read is a slice of the one parse the Store already did this tick —
    // `events::recent` here would put a second full parse on a 1 Hz loop.
    let events: Vec<&(usize, Event)> = match &context {
        Some(Context::Agent(agent)) => tail(app.data.events_by_agent(agent)),
        Some(Context::Path(path)) => tail(app.data.events_by_path(path)),
        None => app.data.feed(FEED_LIMIT).iter().collect(),
    };
    let mut rows = feed_rows(&events);
    // The rate is about the fleet, not about the query: "24 events / 5 min" has
    // to keep meaning "is anything happening at all" while you read one agent's
    // three of them.
    let recent = rate(&events, chrono::Utc::now(), RATE_WINDOW_SECS);

    // Narrowed here, where the feed is projected, so the rows `row_at` and
    // `select` index into are the rows on screen. An event exposes the four
    // fields it is read by: who acted, what they did, on what, and the detail
    // that carries the contention chain.
    let total = rows.len();
    rows.retain(|row| {
        app.filter
            .matches(&[&row.agent, &row.kind, &row.target, &row.detail])
    });
    app.filter.note(rows.len(), total);

    app.activity.rows = rows;
    app.activity.recent = recent;
    app.activity.context = context;

    let index = if app.activity.follow {
        app.activity.rows.len().checked_sub(1)
    } else {
        // By identity: the feed window slides as events arrive, so the row an
        // index points at is a different event a tick later.
        let keys: Vec<String> = app.activity.rows.iter().map(|r| r.id.to_string()).collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let previous = app.activity.selected.map(|id| id.to_string());
        widgets::reselect(&keys, previous.as_deref(), app.activity.table.selected())
    };
    select_index(app, index);
}

fn tail(events: Vec<&(usize, Event)>) -> Vec<&(usize, Event)> {
    let from = events.len().saturating_sub(FEED_LIMIT);
    events[from..].to_vec()
}

/// Enter opens the entity the selected row points at. `h`/`l` chooses which,
/// because an event names two: the agent that acted and the path it acted on.
/// An event with no path falls back to its agent rather than doing nothing.
pub fn on_enter(app: &App) -> Option<View> {
    let row = app
        .activity
        .table
        .selected()
        .and_then(|i| app.activity.rows.get(i))?;
    match app.activity.link {
        Link::Path if !row.target.is_empty() => Some(View::Path(row.target.clone())),
        _ => Some(View::Agent(row.agent.clone())),
    }
}

pub fn handle_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Char('g') => select_index(app, (!app.activity.rows.is_empty()).then_some(0)),
        KeyCode::Char('G') => select_index(app, app.activity.rows.len().checked_sub(1)),
        KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
            app.activity.link = match app.activity.link {
                Link::Path => Link::Agent,
                Link::Agent => Link::Path,
            };
        }
        KeyCode::Char('c') => {
            app.activity.all = !app.activity.all;
            refresh(app);
        }
        _ => return false,
    }
    true
}

pub fn row_at(app: &App, _x: u16, y: u16) -> Option<usize> {
    // 2 header rows: the rate line this screen draws, then the table's own
    // column header. Same rect rendering used — see `render`.
    widgets::row_at(app.content_area, y, 2, app.activity.table.offset())
        .filter(|i| *i < app.activity.rows.len())
}

pub fn select(app: &mut App, index: usize) {
    select_index(app, Some(index));
}

pub fn help() -> &'static str {
    "j/k: move  g/G: top/bottom  h/l: open agent|path  c: all/context"
}

/// Whether the feed is following its tail: the selection is on the last row,
/// however it got there — j/k, a click, the wheel, `G`, or an empty feed that
/// has nowhere else to be. One definition, so the auto-scroll can never fight
/// an operator who has scrolled up.
pub fn following(len: usize, index: Option<usize>) -> bool {
    len == 0 || index == len.checked_sub(1)
}

/// The one place the selection moves, so the follow flag cannot drift from it.
fn select_index(app: &mut App, index: Option<usize>) {
    app.activity.table.select(index);
    app.activity.selected = index.and_then(|i| app.activity.rows.get(i)).map(|r| r.id);
    app.activity.follow = following(app.activity.rows.len(), index);
}

fn move_selection(app: &mut App, delta: isize) {
    let index = widgets::step(
        app.activity.rows.len(),
        app.activity.table.selected(),
        delta,
    );
    select_index(app, index);
}

/// "24 events / 5 min · 312 shown · following · agent: docs-story"
fn header_line(app: &App) -> String {
    let mut parts = vec![
        format!(
            "{} events / {} min",
            app.activity.recent,
            RATE_WINDOW_SECS / 60
        ),
        format!("{} shown", app.activity.rows.len()),
        if app.activity.follow {
            "following".to_string()
        } else {
            "scrolled up — G to follow".to_string()
        },
    ];
    match &app.activity.context {
        Some(Context::Agent(agent)) => parts.push(format!("agent: {agent}")),
        Some(Context::Path(path)) => parts.push(format!("path: {path}")),
        None => {}
    }
    parts.push(
        match app.activity.link {
            Link::Path => "enter: path",
            Link::Agent => "enter: agent",
        }
        .to_string(),
    );
    parts.join(" · ")
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    frame.render_widget(
        Paragraph::new(header_line(app)).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        chunks[0],
    );

    if app.activity.rows.is_empty() {
        frame.render_widget(
            Paragraph::new(match &app.activity.context {
                Some(Context::Agent(agent)) => format!("no events for {agent} — c: whole fleet"),
                Some(Context::Path(path)) => format!("no events on {path} — c: whole fleet"),
                None => "no activity recorded yet".to_string(),
            })
            .style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
        return;
    }

    let header = Row::new(vec!["When", "Agent", "Event", "Target", "Detail"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let selected = app.activity.table.selected();
    let rows: Vec<Row> = app
        .activity
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            feed_widget_row(
                row,
                widgets::is_hovered_not_selected(app.hovered_row, selected, i),
            )
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Percentage(14),
        Constraint::Length(15),
        Constraint::Percentage(24),
        Constraint::Percentage(40),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    // The real TableState, not a clone: its scroll offset is what `row_at`
    // hit-tests against, so a click on a scrolled feed must read the offset
    // this render produced.
    frame.render_stateful_widget(table, chunks[1], &mut app.activity.table);
}

fn feed_widget_row(row: &FeedRow, hovered: bool) -> Row<'static> {
    let widget_row = Row::new(vec![
        Cell::from(row.when.clone()),
        Cell::from(row.agent.clone()),
        Cell::from(row.kind.clone()).style(
            Style::default()
                .fg(row.class.color())
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from(row.target.clone()),
        Cell::from(row.detail.clone()),
    ]);
    if hovered {
        widget_row.style(widgets::hover_style())
    } else {
        widget_row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: usize, at: &str, agent: &str, kind: &str, path: Option<&str>) -> (usize, Event) {
        (
            id,
            Event {
                at: at.to_string(),
                agent: agent.to_string(),
                kind: kind.to_string(),
                path: path.map(str::to_string),
                detail: None,
                ttl_secs: None,
                covers_lines: None,
                actor: None,
                displaced: None,
                content_hash: None,
                chain_hash: None,
                subscriber: None,
                message_id: None,
                protocol_hash: None,
                invoked_from: None,
                collected_from: None,
                scope: None,
                pact_version: None,
                head: None,
                holder: None,
                holder_remaining_secs: None,
                holder_branch: None,
                holder_worktree: None,
                ..Default::default()
            },
        )
    }

    fn agent_info(name: &str) -> AgentInfo {
        AgentInfo {
            name: name.to_string(),
            last_seen: "2026-01-01T00:00:00Z".to_string(),
            leases_held: 0,
            lease_events: 0,
            messages_sent: 0,
            messages_received: 0,
            name_valid: true,
            harness: None,
            model: None,
            idle_secs: None,
        }
    }

    /// Contention must not read as traffic — that separation is the whole
    /// reason this screen colours at all.
    #[test]
    fn every_kind_a_real_log_carries_lands_in_its_own_class() {
        for kind in ["acquired", "released", "renewed", "watched"] {
            assert_eq!(classify(kind), Kind::Traffic, "{kind}");
        }
        for kind in ["refused", "stolen", "expired", "force-released"] {
            assert_eq!(classify(kind), Kind::Contention, "{kind}");
        }
        for kind in ["notified", "annotation"] {
            assert_eq!(classify(kind), Kind::Machinery, "{kind}");
        }
        // A kind this build has never heard of is never guessed into a class.
        assert_eq!(classify("unwatched"), Kind::Other);
        assert_ne!(Kind::Traffic.color(), Kind::Contention.color());
    }

    /// The fields that make a refusal followable are the ones a truncated
    /// detail column would drop first, so they go before the free text.
    #[test]
    fn a_refusal_keeps_its_holder_and_remaining_time_ahead_of_the_note() {
        let (_, mut e) = event(1, "2026-01-01T00:00:00Z", "docs-story", "refused", None);
        e.holder = Some("quern".to_string());
        e.holder_remaining_secs = Some(355);
        e.detail = Some(
            "held by another agent, and here is a very long note that will \
                         certainly be truncated away before it ends"
                .to_string(),
        );
        let detail = detail_of(&e);
        assert!(detail.starts_with("held by quern, 5m55s left"), "{detail}");
        assert!(detail.chars().count() <= 60, "{detail}");
    }

    #[test]
    fn a_notice_keeps_the_subscriber_and_the_message_it_created() {
        let (_, mut e) = event(1, "2026-01-01T00:00:00Z", "quern", "notified", None);
        e.subscriber = Some("docs-story".to_string());
        e.message_id = Some("pact-msg-abc".to_string());
        assert_eq!(detail_of(&e), "to docs-story · pact-msg-abc");
    }

    /// An idle fleet must read as idle: the rate counts the window, not the
    /// log.
    #[test]
    fn the_rate_counts_only_the_window_and_never_an_unparsable_stamp() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T01:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let events = [
            event(1, "2026-01-01T00:00:00Z", "a", "acquired", None), // an hour ago
            event(2, "2026-01-01T00:56:00Z", "a", "released", None), // 4m ago
            event(3, "2026-01-01T00:59:30Z", "b", "acquired", None), // 30s ago
            event(4, "not-a-timestamp", "b", "acquired", None),
        ];
        let refs: Vec<&(usize, Event)> = events.iter().collect();
        assert_eq!(rate(&refs, now, RATE_WINDOW_SECS), 2);
        // A quiet fleet reads as quiet rather than as its whole history.
        assert_eq!(rate(&refs, now, 10), 0);
    }

    #[test]
    fn the_feed_reads_oldest_first_the_way_pact_log_does() {
        let events = [
            event(1, "2026-01-01T00:00:00Z", "a", "acquired", Some("src/a.rs")),
            event(2, "2026-01-01T00:01:00Z", "b", "refused", Some("src/a.rs")),
        ];
        let refs: Vec<&(usize, Event)> = events.iter().collect();
        let rows = feed_rows(&refs);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[1].kind, "refused", "the newest event is last");
        assert_eq!(rows[1].class, Kind::Contention);
        assert_eq!(rows[0].target, "src/a.rs");
    }

    /// The acceptance criterion, as the one predicate everything that moves
    /// the cursor goes through: scrolling up stops the follow, returning to the
    /// bottom resumes it.
    #[test]
    fn scrolling_up_stops_the_follow_and_the_bottom_resumes_it() {
        assert!(following(3, Some(2)), "on the last row: following");
        assert!(!following(3, Some(1)), "scrolled up: not following");
        assert!(!following(3, Some(0)));
        assert!(following(3, Some(2)), "back at the bottom: following again");
        // Nothing to follow yet, and nothing to fight over either — an empty
        // feed must not open scrolled up.
        assert!(following(0, None));
        assert!(State::default().follow);
    }

    /// The narrowing is resolved against the roster, not assumed from Fleet's
    /// selection — which is a path in this build and an agent name in the next.
    #[test]
    fn the_context_is_whatever_the_selection_turns_out_to_name() {
        let roster = [agent_info("docs-story"), agent_info("quern")];
        assert_eq!(
            context_of(Some("docs-story"), &roster, false),
            Some(Context::Agent("docs-story".into()))
        );
        assert_eq!(
            context_of(Some("src/lease.rs"), &roster, false),
            Some(Context::Path("src/lease.rs".into()))
        );
        assert_eq!(context_of(None, &roster, false), None);
        assert_eq!(context_of(Some(""), &roster, false), None);
        // `c` widens back to the whole fleet, which is otherwise unreachable:
        // Fleet always has something selected.
        assert_eq!(context_of(Some("docs-story"), &roster, true), None);
    }
}
