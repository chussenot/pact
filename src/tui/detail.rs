//! Path and Agent detail — the two drill-ins Enter opens.
//!
//! pact-pyt.4 (a path: who holds it, who held it before, what was said ABOUT
//! it, who is subscribed, who is blocked on it), pact-pyt.5 (an agent: what it
//! holds, what it did, its mail and whether anyone read it) and the detail half
//! of pact-pyt.7 (the waiting-on rows on both screens).
//!
//! Both screens are one flat list of lines, and **every entity reference on a
//! line is a link**: an agent name opens the agent view, a path opens the path
//! view, a message id opens its thread. That is what makes this a navigable
//! graph rather than two dead-end panels — [`on_enter`] returns the view the
//! selected line points at and `mod.rs` pushes it, so Esc comes back to
//! wherever the operator noticed the name.
//!
//! Read-only, structurally: every fact comes off `app.data` (parsed once per
//! tick — nothing here opens a file), `on_enter` takes `&App`, and nothing
//! marks a message read or collects an expired lock. A question must not change
//! its own answer.

use chrono::{DateTime, Utc};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use ratatui::crossterm::event::KeyCode;

use super::data::{Blocked, Store, DEFAULT_GRACE_SECS};
use super::nav::View;
use super::widgets;
use super::App;
use crate::agents::AgentInfo;
use crate::lease::human_secs;
use crate::msg::Message;

/// How much history a detail view shows before it is just the Activity screen
/// with a filter. The newest N, in chronological order, and the heading says
/// what was left out rather than trailing off silently.
const MAX_EVENTS: usize = 20;
/// The same, for a mailbox. Newest first here: "did the last one land" is the
/// question `pact msg sent` answers and the reason agents re-send duplicates.
const MAX_MESSAGES: usize = 10;

/// One rendered line, and what it points at.
struct Row {
    text: String,
    style: Style,
    /// What Enter opens from this line. `None` for a heading or a plain fact.
    link: Option<View>,
}

impl Row {
    fn heading(text: impl Into<String>) -> Row {
        Row {
            text: text.into(),
            style: Style::default().add_modifier(Modifier::BOLD),
            link: None,
        }
    }

    fn fact(text: impl Into<String>) -> Row {
        Row {
            text: text.into(),
            style: Style::default(),
            link: None,
        }
    }

    /// Says a section is empty, in words. An empty box looks like a dashboard
    /// that failed to load; "nobody is subscribed to this path" is an answer.
    fn nothing(text: impl Into<String>) -> Row {
        Row {
            text: text.into(),
            style: Style::default().fg(Color::DarkGray),
            link: None,
        }
    }

    /// Something the operator should look at twice.
    fn flag(text: impl Into<String>) -> Row {
        Row {
            text: text.into(),
            style: Style::default().fg(Color::Yellow),
            link: None,
        }
    }

    fn link(text: impl Into<String>, view: View) -> Row {
        Row {
            text: text.into(),
            style: Style::default().fg(Color::Cyan),
            link: Some(view),
        }
    }
}

#[derive(Default)]
pub struct State {
    rows: Vec<Row>,
    list: ListState,
    /// The line the operator selected, by its text — so a refresh that adds a
    /// row above it does not slide the cursor onto a different one. See
    /// [`widgets::reselect`].
    selected: Option<String>,
    /// Which view these rows were built for. A drill-in from here is a
    /// different entity, so its selection starts fresh instead of inheriting a
    /// row number from the view it was opened from.
    built_for: Option<View>,
}

pub fn refresh(app: &mut App) {
    let view = app.nav.current().clone();
    let now = Utc::now();
    let rows = match &view {
        View::Path(path) => path_rows(&app.data, path, now),
        View::Agent(agent) => agent_rows(&app.data, agent, now),
        // Nothing else is dispatched here — `View::screen()` sends only these
        // two to this module.
        _ => Vec::new(),
    };

    if app.detail.built_for.as_ref() != Some(&view) {
        app.detail.selected = None;
        app.detail.list = ListState::default();
        app.detail.built_for = Some(view);
    }
    app.detail.rows = rows;

    let index = match app.detail.selected.clone() {
        Some(previous) => {
            let keys: Vec<&str> = app.detail.rows.iter().map(|r| r.text.as_str()).collect();
            widgets::reselect(&keys, Some(previous.as_str()), app.detail.list.selected())
        }
        // Land on the first thing Enter can follow, not on the title: this
        // screen exists to be walked through.
        None => app.detail.rows.iter().position(|r| r.link.is_some()),
    };
    select_index(app, index);
}

/// Enter follows the link on the selected line — an agent name to the agent
/// view, a path to the path view, a message id to its thread.
pub fn on_enter(app: &App) -> Option<View> {
    app.detail
        .list
        .selected()
        .and_then(|i| app.detail.rows.get(i))
        .and_then(|row| row.link.clone())
}

pub fn handle_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        _ => return false,
    }
    true
}

pub fn row_at(app: &App, _x: u16, y: u16) -> Option<usize> {
    widgets::row_at(app.content_area, y, 0, app.detail.list.offset())
        .filter(|i| *i < app.detail.rows.len())
}

pub fn select(app: &mut App, index: usize) {
    select_index(app, Some(index));
}

pub fn help() -> &'static str {
    "j/k: next link  enter: follow it"
}

fn select_index(app: &mut App, index: Option<usize>) {
    app.detail.list.select(index);
    app.detail.selected = index
        .and_then(|i| app.detail.rows.get(i))
        .map(|row| row.text.clone());
}

/// Step to the next *link*, wrapping. Headings and plain facts are skipped:
/// there is nothing to do on them, and a cursor that stops on every line of
/// prose makes the one key this screen has feel broken. A view with no links at
/// all comes full circle and stays put.
fn move_selection(app: &mut App, delta: isize) {
    let len = app.detail.rows.len();
    if len == 0 {
        return;
    }
    let mut index = app.detail.list.selected().unwrap_or(0) as isize;
    for _ in 0..len {
        index = (index + delta).rem_euclid(len as isize);
        if app.detail.rows[index as usize].link.is_some() {
            break;
        }
    }
    select_index(app, Some(index as usize));
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.detail.rows.is_empty() {
        frame.render_widget(
            Paragraph::new("nothing to show here — press r to refresh")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .detail
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let mut style = row.style;
            if widgets::is_hovered_not_selected(app.hovered_row, app.detail.list.selected(), index)
            {
                style = style.patch(widgets::hover_style());
            }
            ListItem::new(row.text.clone()).style(style)
        })
        .collect();

    // Taken and put back rather than cloned: rendering is what updates the
    // scroll offset, and `row_at` reads that same offset to decide which row a
    // click landed on. A clone would hit-test a scrolled list against offset 0.
    let mut state = std::mem::take(&mut app.detail.list);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> "),
        area,
        &mut state,
    );
    app.detail.list = state;
}

// --------------------------------------------------------------- the path view

/// Everything pact knows about one file. Takes the read model and a clock
/// rather than `&App`, so the whole screen is a pure function of parsed state.
fn path_rows(data: &Store, path: &str, now: DateTime<Utc>) -> Vec<Row> {
    let mut rows = vec![Row::heading(format!("path  {path}")), Row::fact("")];

    rows.push(Row::heading("held"));
    let live: Vec<_> = data
        .leases()
        .iter()
        .filter(|entry| entry.lease.path == path)
        .collect();
    if live.is_empty() {
        rows.push(Row::nothing("  nobody holds it now"));
    }
    for entry in live {
        // `state_label` and not a state of our own: it is what `pact lease ls`
        // prints, including the SUSPECT band for a holder that has gone quiet
        // (pact-mqw.6) — a stalled holder must not read as a healthy one here
        // either.
        let mut line = format!(
            "  {}  {}  held {}",
            entry.lease.agent,
            entry.state_label(),
            human_secs(entry.age_secs)
        );
        if let Some(note) = &entry.lease.note {
            line.push_str(&format!("  — {note}"));
        }
        rows.push(Row::link(line, View::Agent(entry.lease.agent.clone())));
    }
    // The question a lock cannot answer once it is gone, and the reason this
    // screen exists: who had this file last?
    match data.owner_of(path) {
        Some(event) => rows.push(Row::link(
            format!(
                "  last in the log: {} ({} at {})",
                event.agent,
                event.kind,
                stamp(&event.at)
            ),
            View::Agent(event.agent.clone()),
        )),
        None => rows.push(Row::nothing("  no custody event recorded for this path")),
    }

    rows.push(Row::fact(""));
    rows.push(Row::heading("waiting on it"));
    let waiting = data.waiting_on(now, DEFAULT_GRACE_SECS);
    let blocked: Vec<&Blocked> = waiting
        .blocked
        .iter()
        .filter(|b| b.path == path && b.live())
        .collect();
    if blocked.is_empty() {
        rows.push(Row::nothing("  nobody is blocked on it"));
    }
    for b in blocked {
        rows.push(Row::link(
            blocked_line(&b.agent, b),
            View::Agent(b.agent.clone()),
        ));
    }
    rows.push(Row::nothing(window_line(waiting.grace_secs)));

    rows.push(Row::fact(""));
    rows.push(Row::heading("watchers"));
    let subscribers = data.subscribers(path, now);
    if subscribers.is_empty() {
        rows.push(Row::nothing(
            "  nobody is subscribed — a release here notifies no one",
        ));
    }
    for agent in subscribers {
        rows.push(Row::link(
            format!("  {agent}"),
            View::Agent(agent.to_string()),
        ));
    }

    rows.push(Row::fact(""));
    rows.push(Row::heading("messages about this path"));
    // The important one (pact-4tj): a message sent with `--to-owner-of` is
    // tagged with the path and outlives the agent it resolved to. 30 of one
    // fleet's messages were exactly that and no surface showed them — and a
    // message waiting on a path is usually the reason the last agent stopped.
    let about = data.messages_about(path);
    if about.is_empty() {
        rows.push(Row::nothing(
            "  none — nothing was ever addressed to this file",
        ));
    }
    for m in about {
        rows.push(Row::link(
            message_line(m, &m.from),
            View::Thread(m.id.clone()),
        ));
    }

    rows.push(Row::fact(""));
    let events = data.events_by_path(path);
    rows.push(Row::heading(history_heading(
        "custody history",
        events.len(),
    )));
    if events.is_empty() {
        rows.push(Row::nothing("  nothing has ever happened to this path"));
    }
    for (_, event) in tail(&events) {
        rows.push(Row::link(
            format!(
                "  {}  {:<9}  {}{}",
                stamp(&event.at),
                event.kind,
                event.agent,
                event_detail(event)
            ),
            View::Agent(event.agent.clone()),
        ));
    }
    rows
}

// -------------------------------------------------------------- the agent view

/// Everything about one actor — reached from wherever the operator noticed the
/// name: a holder column, an event line, a refusal, a message's from/to.
fn agent_rows(data: &Store, agent: &str, now: DateTime<Utc>) -> Vec<Row> {
    let mut rows = vec![Row::heading(format!("agent  {agent}"))];
    match data.roster().iter().find(|info| info.name == agent) {
        Some(info) => {
            rows.push(Row::fact(format!(
                "  last seen {} · {} lease events · {} held now · {} sent · {} received",
                age(&info.last_seen, now),
                info.lease_events,
                info.leases_held,
                info.messages_sent,
                info.messages_received
            )));
            rows.push(identity_row(info));
        }
        None => rows.push(Row::nothing("  no trace of this name in this repo")),
    }

    rows.push(Row::fact(""));
    rows.push(Row::heading("holds now"));
    let held: Vec<_> = data
        .leases()
        .iter()
        .filter(|entry| entry.lease.agent == agent)
        .collect();
    if held.is_empty() {
        rows.push(Row::nothing("  no live leases"));
    }
    for entry in held {
        rows.push(Row::link(
            format!(
                "  {}  {}  held {}",
                entry.lease.path,
                entry.state_label(),
                human_secs(entry.age_secs)
            ),
            View::Path(entry.lease.path.clone()),
        ));
    }

    rows.push(Row::fact(""));
    rows.push(Row::heading("blocked on"));
    let waiting = data.waiting_on(now, DEFAULT_GRACE_SECS);
    let blocked: Vec<&Blocked> = waiting
        .blocked
        .iter()
        .filter(|b| b.agent == agent && b.live())
        .collect();
    if blocked.is_empty() {
        rows.push(Row::nothing("  not blocked on anything"));
    }
    for b in blocked {
        rows.push(Row::link(
            blocked_line(&b.path, b),
            View::Path(b.path.clone()),
        ));
    }
    rows.push(Row::nothing(window_line(waiting.grace_secs)));

    rows.push(Row::fact(""));
    let sent = data.sent(agent);
    rows.push(Row::heading(history_heading("sent", sent.len())));
    if sent.is_empty() {
        rows.push(Row::nothing("  nothing sent"));
    }
    // Newest first, and each line says whether the recipient read it. That is
    // the question `pact msg sent` answers, and not being able to ask it is why
    // agents re-send messages that already landed.
    for m in sent.iter().take(MAX_MESSAGES) {
        rows.push(Row::link(
            message_line(m, &format!("to {}", m.to)),
            View::Thread(m.id.clone()),
        ));
    }

    rows.push(Row::fact(""));
    let inbox = data.inbox(agent);
    rows.push(Row::heading(history_heading("received", inbox.len())));
    if inbox.is_empty() {
        rows.push(Row::nothing("  nothing received"));
    }
    for m in inbox.iter().rev().take(MAX_MESSAGES) {
        rows.push(Row::link(
            message_line(m, &format!("from {}", m.from)),
            View::Thread(m.id.clone()),
        ));
    }

    rows.push(Row::fact(""));
    let events = data.events_by_agent(agent);
    rows.push(Row::heading(history_heading("events", events.len())));
    if events.is_empty() {
        rows.push(Row::nothing(
            "  no lease events — this name has never run pact",
        ));
    }
    for (_, event) in tail(&events) {
        let line = format!(
            "  {}  {:<9}  {}{}",
            stamp(&event.at),
            event.kind,
            event.path.as_deref().unwrap_or(""),
            event_detail(event)
        );
        match &event.path {
            Some(path) => rows.push(Row::link(line, View::Path(path.clone()))),
            None => rows.push(Row::fact(line)),
        }
    }
    rows
}

// ------------------------------------------------------------------- fragments

/// Does anyone actually answer to this name?
///
/// Being ADDRESSED proves only that somebody typed it, which is exactly what a
/// typo looks like — counting it is how one typo'd send used to certify itself
/// forever (pact-rnc.5). `name_valid` is the stronger flag: a name that cannot
/// pass `identity::validate` is one no pact process could ever have run under,
/// however much traffic the store shows for it.
fn identity_row(info: &AgentInfo) -> Row {
    if !info.name_valid {
        return Row::flag("  INVALID NAME — no pact process could have run under it");
    }
    if info.answers() {
        Row::fact("  answers to its name — something has run pact under it")
    } else {
        Row::flag("  only ever ADDRESSED — nothing has ever run under this name")
    }
}

/// One edge of the contention graph, from whichever end you are looking at:
/// `subject` is the blocked agent on a path view and the path on an agent view.
fn blocked_line(subject: &str, b: &Blocked) -> String {
    let holder = match (&b.holder, b.holder_remaining_secs) {
        // The number to wait on. `ttl_secs` on a refusal is the REFUSED agent's
        // own ask, not the holder's remaining time, which is how one agent
        // retried 33 times against a median 355s of hold (pact-1gv.1).
        (Some(holder), Some(remaining)) => {
            format!("held by {holder}, {} left", human_secs(remaining))
        }
        (Some(holder), None) => format!("held by {holder}"),
        (None, _) => "holder not recorded".to_string(),
    };
    format!(
        "  {subject} — {holder}, waiting {}, {} refusal(s), {}",
        human_secs(b.waited_secs),
        b.refusals,
        subscription_state(b)
    )
}

/// Subscribed, or polling? The distinction pact-pyt.7 exists for: 24 refusals
/// in one run came from agents that had ALREADY subscribed and polled anyway,
/// so "subscribed" alone is not the whole answer.
fn subscription_state(b: &Blocked) -> &'static str {
    match (b.subscribed, b.retry_storm) {
        (true, true) => "subscribed, and polling anyway (retry storm)",
        (true, false) => "subscribed",
        (false, true) => "POLLING, never subscribed (retry storm)",
        (false, false) => "not subscribed — a release will not reach it",
    }
}

/// The staleness bound, on screen. A refusal outlives the hold it NAMED by the
/// grace and no longer, and an unqualified list of old refusals would present
/// history as people currently stuck.
fn window_line(grace_secs: i64) -> String {
    format!(
        "  (a refusal counts as live until the hold it named runs out, plus {})",
        human_secs(grace_secs)
    )
}

/// A mailbox line: the id (which opens the thread), who it is with, the
/// subject, and whether the recipient has read it.
fn message_line(m: &Message, counterpart: &str) -> String {
    let read = if m.read_by.iter().any(|reader| reader == &m.to) {
        format!("read by {}", m.to)
    } else {
        format!("UNREAD by {}", m.to)
    };
    format!(
        "  {}  {}  {}  [{}]",
        m.id,
        counterpart,
        m.subject.as_deref().unwrap_or("(no subject)"),
        read
    )
}

/// What a row adds beyond kind and actor — the fields that are only on some
/// kinds, in the same words the CLI uses.
fn event_detail(event: &crate::events::Event) -> String {
    let mut out = String::new();
    if let Some(holder) = &event.holder {
        out.push_str(&format!("  held by {holder}"));
        if let Some(remaining) = event.holder_remaining_secs {
            out.push_str(&format!(", {} left", human_secs(remaining)));
        }
    }
    if let Some(displaced) = &event.displaced {
        out.push_str(&format!("  displaced {displaced}"));
    }
    if let Some(subscriber) = &event.subscriber {
        out.push_str(&format!("  -> {subscriber}"));
    }
    if let Some(detail) = &event.detail {
        out.push_str(&format!("  {detail}"));
    }
    out
}

fn history_heading(what: &str, total: usize) -> String {
    if total > MAX_EVENTS {
        format!("{what} (last {MAX_EVENTS} of {total})")
    } else {
        format!("{what} ({total})")
    }
}

/// The newest [`MAX_EVENTS`] rows, back in chronological order — a custody
/// history read top to bottom is the story of the file.
fn tail<'a, T>(rows: &[&'a T]) -> Vec<&'a T> {
    let mut out: Vec<&T> = rows.iter().rev().take(MAX_EVENTS).copied().collect();
    out.reverse();
    out
}

/// `14:37:25`. The stored stamps are RFC3339 and the date is noise on a screen
/// showing a fleet that is running now; an unparsable one is shown verbatim
/// rather than dropped.
fn stamp(at: &str) -> String {
    match DateTime::parse_from_rfc3339(at) {
        Ok(t) => t.with_timezone(&Utc).format("%H:%M:%S").to_string(),
        Err(_) => at.to_string(),
    }
}

fn age(at: &str, now: DateTime<Utc>) -> String {
    match DateTime::parse_from_rfc3339(at) {
        Ok(t) => format!(
            "{} ago",
            human_secs((now - t.with_timezone(&Utc)).num_seconds())
        ),
        Err(_) => at.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A repo with the fixture chain this repository's own log recorded:
    /// orchestrator holds docs/tui.md, docs-story is refused it and subscribes
    /// 17 seconds later.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".pact")).unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        write(
            tmp.path(),
            "events.jsonl",
            &[
                serde_json::json!({
                    "at": "2026-08-14T14:30:00+00:00", "agent": "orchestrator",
                    "kind": "acquired", "path": "docs/tui.md",
                }),
                serde_json::json!({
                    "at": "2026-08-14T14:37:25+00:00", "agent": "docs-story",
                    "kind": "refused", "path": "docs/tui.md",
                    "holder": "orchestrator", "holder_remaining_secs": 2393,
                }),
                serde_json::json!({
                    "at": "2026-08-14T14:37:42+00:00", "agent": "docs-story",
                    "kind": "watched", "path": "docs/tui.md",
                }),
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

    fn write(root: &Path, name: &str, lines: &[serde_json::Value]) {
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(root.join(".pact").join(name), body).unwrap();
    }

    fn store(root: &Path) -> Store {
        let mut store = Store::default();
        store.refresh(root);
        store
    }

    fn now() -> DateTime<Utc> {
        at("2026-08-14T14:40:00+00:00")
    }

    fn at(stamp: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(stamp)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn text(rows: &[Row]) -> String {
        rows.iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn links(rows: &[Row]) -> Vec<View> {
        rows.iter().filter_map(|row| row.link.clone()).collect()
    }

    /// The whole tree under `root`, so a "read-only view" can be shown to be
    /// one rather than asserted to be.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push((path.clone(), fs::read(&path).unwrap_or_default()));
                }
            }
        }
        out.sort();
        out
    }

    fn app_on(root: &Path, view: View) -> App {
        let mut app = App::new(root.to_path_buf(), Some("operator".to_string()));
        app.nav.push(view);
        app.refresh_current_view();
        app
    }

    /// pact-pyt.4's first acceptance criterion, minus the parts with their own
    /// test below: the holder, the prior holder, and the block on it.
    #[test]
    fn a_path_view_names_who_holds_it_who_held_it_and_who_is_blocked_on_it() {
        let tmp = repo();
        let rows = path_rows(&store(tmp.path()), "docs/tui.md", now());
        let rendered = text(&rows);

        assert!(rendered.starts_with("path  docs/tui.md"), "{rendered}");
        // No lock file on disk, so the log is the only thing that knows.
        assert!(rendered.contains("nobody holds it now"), "{rendered}");
        assert!(
            rendered.contains("last in the log: orchestrator (acquired at 14:30:00)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("docs-story — held by orchestrator, 39m53s left"),
            "{rendered}"
        );
        assert!(rendered.contains("2m35s"), "how long it has waited");
        // .7: it subscribed, so this is contention, not a poll loop.
        assert!(rendered.contains("subscribed"), "{rendered}");
        assert!(!rendered.contains("retry storm"), "{rendered}");
        // The custody history, oldest first.
        assert!(rendered.contains("14:37:25  refused"), "{rendered}");
    }

    /// The other acceptance criterion, and the reason it is one: two empty
    /// boxes look exactly like a dashboard that failed to load.
    #[test]
    fn a_path_with_no_messages_and_no_watchers_says_so() {
        let tmp = repo();
        let rows = path_rows(&store(tmp.path()), "src/lease.rs", now());
        let rendered = text(&rows);

        assert!(rendered.contains("nobody is subscribed"), "{rendered}");
        assert!(
            rendered.contains("nothing was ever addressed to this file"),
            "{rendered}"
        );
        assert!(rendered.contains("no custody event recorded"), "{rendered}");
        assert!(rendered.contains("nobody is blocked on it"), "{rendered}");
    }

    /// A prefix watch covers what is under it — the coverage rule is
    /// `watch::was_subscribed_at`'s, never a second `starts_with` here.
    #[test]
    fn a_prefix_watch_makes_its_subscriber_a_watcher_of_everything_under_it() {
        let tmp = repo();
        write(
            tmp.path(),
            "watches.jsonl",
            &[serde_json::json!({
                "at": "2026-08-14T14:00:00+00:00", "agent": "readmodel",
                "kind": "watch", "path": "src/tui", "prefix": true,
            })],
        );
        let data = store(tmp.path());

        let covered = text(&path_rows(&data, "src/tui/detail.rs", now()));
        assert!(covered.contains("readmodel"), "{covered}");
        // `src/tuinot.rs` is not under `src/tui/` — the documented trap.
        let near_miss = text(&path_rows(&data, "src/tuinot.rs", now()));
        assert!(near_miss.contains("nobody is subscribed"), "{near_miss}");
    }

    /// pact-pyt.4's headline: a message tagged with `--to-owner-of` follows the
    /// file, and this is the only surface that shows it.
    #[test]
    fn messages_about_a_path_are_listed_and_open_their_thread() {
        let tmp = repo();
        let sent = msg::send(
            tmp.path(),
            "spine",
            &["fleet".to_string()],
            msg::Draft {
                thread: None,
                subject: Some("renamed foo()"),
                body: "the signature lost its second parameter",
                about: &["docs/tui.md".to_string()],
                notice: false,
            },
        )
        .unwrap();
        let id = sent[0].id.clone();

        let rows = path_rows(&store(tmp.path()), "docs/tui.md", now());
        let rendered = text(&rows);
        assert!(rendered.contains("renamed foo()"), "{rendered}");
        assert!(rendered.contains("UNREAD by fleet"), "{rendered}");
        assert!(links(&rows).contains(&View::Thread(id)), "{rendered}");
    }

    /// .7's bound, stated on screen: an hour later the same refusal is history,
    /// not somebody currently stuck.
    #[test]
    fn an_old_refusal_is_not_presented_as_a_live_block_and_the_window_is_stated() {
        let tmp = repo();
        let rows = path_rows(
            &store(tmp.path()),
            "docs/tui.md",
            at("2026-08-14T16:00:00+00:00"),
        );
        let rendered = text(&rows);
        assert!(rendered.contains("nobody is blocked on it"), "{rendered}");
        assert!(
            rendered.contains("plus 5m0s"),
            "the staleness bound must be on screen: {rendered}"
        );
    }

    /// pact-pyt.5: an agent that answers to its name, versus one that has only
    /// ever been addressed — which is exactly what a typo looks like.
    #[test]
    fn the_agent_view_distinguishes_answering_from_only_ever_addressed() {
        let tmp = repo();
        msg::send(
            tmp.path(),
            "docs-story",
            &["orchestratr".to_string()],
            msg::Draft {
                thread: None,
                subject: Some("typo"),
                body: "sent to a name nobody runs under",
                about: &[],
                notice: false,
            },
        )
        .unwrap();
        let data = store(tmp.path());

        let real = text(&agent_rows(&data, "docs-story", now()));
        assert!(real.contains("answers to its name"), "{real}");
        assert!(real.contains("2 lease events"), "{real}");

        let typo = text(&agent_rows(&data, "orchestratr", now()));
        assert!(typo.contains("only ever ADDRESSED"), "{typo}");
        // And the stronger flag, for a name no pact process could ever hold.
        let bogus = text(&agent_rows(&data, "not a valid name", now()));
        assert!(
            bogus.contains("no trace of this name"),
            "unseen names have no row at all: {bogus}"
        );
    }

    /// The question `pact msg sent` answers, and the reason agents re-send
    /// messages that already landed.
    #[test]
    fn the_agent_view_says_whether_a_sent_message_was_read() {
        let tmp = repo();
        let sent = msg::send(
            tmp.path(),
            "docs-story",
            &["orchestrator".to_string()],
            msg::Draft {
                thread: None,
                subject: Some("docs/tui.md is stale"),
                body: "three tabs, and enter is no longer the release key",
                about: &[],
                notice: false,
            },
        )
        .unwrap();

        let before = text(&agent_rows(&store(tmp.path()), "docs-story", now()));
        assert!(before.contains("UNREAD by orchestrator"), "{before}");

        msg::mark_read_by_id(tmp.path(), "orchestrator", &sent[0].id).unwrap();
        let after = text(&agent_rows(&store(tmp.path()), "docs-story", now()));
        assert!(after.contains("read by orchestrator"), "{after}");
        assert!(!after.contains("UNREAD"), "{after}");
    }

    /// The whole design in one assertion: every entity reference is a link, so
    /// a path opens its holder and the holder opens its work.
    #[test]
    fn enter_follows_the_link_on_the_selected_line() {
        let tmp = repo();
        let mut app = app_on(tmp.path(), View::Path("docs/tui.md".to_string()));

        // The cursor lands on the first followable line, not on the title.
        assert_eq!(
            on_enter(&app),
            Some(View::Agent("orchestrator".to_string())),
            "{}",
            text(&app.detail.rows)
        );

        // j walks to the next link, never stopping on a heading.
        handle_key(&mut app, KeyCode::Char('j'));
        let selected = app.detail.list.selected().unwrap();
        assert!(app.detail.rows[selected].link.is_some());
        assert_eq!(on_enter(&app), app.detail.rows[selected].link.clone());

        // And the agent view links back to the path, which is what makes this a
        // graph rather than two dead ends.
        let mut app = app_on(tmp.path(), View::Agent("docs-story".to_string()));
        refresh(&mut app);
        assert!(
            links(&app.detail.rows).contains(&View::Path("docs/tui.md".to_string())),
            "{}",
            text(&app.detail.rows)
        );
    }

    /// Opening a drill-in must not collect an expired lock, mark a message
    /// read, or write anything else: a question must not change its answer.
    #[test]
    fn opening_a_path_view_mutates_nothing_on_disk() {
        let tmp = repo();
        let mut app = app_on(tmp.path(), View::Path("docs/tui.md".to_string()));
        let before = snapshot(tmp.path());

        app.refresh_current_view();
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 110, 30), &mut app))
            .unwrap();
        handle_key(&mut app, KeyCode::Char('j'));

        assert_eq!(snapshot(tmp.path()), before, "the view wrote to .pact/");
    }

    /// A click selects the row it landed on, using the same offset rendering
    /// just scrolled to — the `tab_rects` discipline applied to rows.
    #[test]
    fn a_click_lands_on_the_row_it_hit() {
        let tmp = repo();
        let mut app = app_on(tmp.path(), View::Path("docs/tui.md".to_string()));
        app.content_area = Rect::new(0, 3, 110, 20);

        assert_eq!(row_at(&app, 0, 3), Some(0));
        assert_eq!(row_at(&app, 0, 6), Some(3));
        // Past the last row is nothing, not the closest row.
        assert_eq!(row_at(&app, 0, 200), None);

        select(&mut app, 3);
        assert_eq!(app.detail.list.selected(), Some(3));
        assert_eq!(
            app.detail.selected.as_deref(),
            Some(app.detail.rows[3].text.as_str())
        );
    }
}
