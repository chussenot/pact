//! Health: is the RUN healthy, not just the repository.
//!
//! Owned by pact-pyt.8. The Doctor tab this replaces was 25 checks about
//! repository setup — gitignore, `AGENTS.md` staleness, symlinks, worktree
//! scope. Load-bearing exactly once, when a repo is onboarded, and noise for
//! the eight hours after, while occupying a quarter of the top-level surface.
//! Meanwhile the checks that are about the FLEET — stale holds, retry storms,
//! silent contention, double wins, claims that name somebody else's bead —
//! lived in `pact audit`, were exposed over MCP, and were absent from the
//! human's dashboard entirely.
//!
//! So: one screen, three sections.
//!
//! - **SETUP** — today's doctor checks, collapsed to a one-line verdict while
//!   everything is green and expanded the moment it is not.
//! - **BEHAVIOUR** — the offline audit checks, over the live event log.
//! - **DEEP** — the ones that read git history, behind a keypress.
//!
//! Three rules decide what runs where, and each one is a trap this repo has
//! already hit:
//!
//! 1. **Doctor stays lazy.** It spawns `bd` twice (`--version`, and `config get
//!    audit.enabled` for the sidecar check), so it runs on the first visit to
//!    this screen and on `d` — never on the 1 Hz tick that also lands in
//!    [`refresh`]. The dashboard used to spawn `bd` ~10x/second for a lesser
//!    reason than this.
//! 2. **No git subprocess on the refresh timer.** `commit-correlation` shells
//!    out through `git_history.rs`, so it and `merge-divergence` sit behind `g`
//!    and say on screen that running them costs something.
//! 3. **Nothing here re-reads what the read model already has.**
//!    `retry-storm` comes off [`super::data::Store::storms`], which is
//!    `pact audit --check retry-storm`'s own verdict computed once per tick —
//!    two implementations of "is this a poll loop" would disagree.

use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;

use super::nav::View;
use super::widgets;
use super::App;
use crate::audit::{self, Check, CheckReport, RetryStorm};
use crate::doctor;
use crate::lease::human_secs;

/// How often the setup checks are re-run from a screen that is not Health.
///
/// The indicator has to be current from any screen, and doctor spawns `bd`
/// twice — so, exactly like the Messages unread badge's `UNREAD_INTERVAL`, it
/// gets a deliberately slower clock of its own and never lands on the 1 Hz one.
const SETUP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct State {
    /// `None` until the setup checks have been run at least once — they shell
    /// out, so they are never on the refresh timer.
    pub report: Option<doctor::DoctorReport>,
    /// When they last ran. See [`SETUP_INTERVAL`].
    pub last_setup: Option<Instant>,
    /// Show every setup check, not just the ones that have something to say.
    pub show_all_setup: bool,

    /// The offline audit checks over the current event log.
    pub behaviour: Vec<Group>,
    /// How long the event log was when `behaviour` was computed.
    ///
    /// ponytail: the log is append-only, so its length is a sound "did the
    /// verdict move" key and costs nothing — the read model has already parsed
    /// it. Ceiling: `claim-lease-divergence` also reads
    /// `.beads/interactions.jsonl` and `silent-contention` also reads
    /// `.pact/watches.jsonl`, so a change confined to one of those is not
    /// noticed until the next event lands. Stamp those two files as well if
    /// that ever matters.
    behaviour_events: Option<usize>,

    /// The git-backed checks. Empty until `g`.
    pub deep: Vec<Group>,
    deep_note: Option<String>,

    pub list: ListState,
    /// The row the operator is on, by identity rather than by index — the list
    /// is rebuilt on every frame, and an index means a different row the moment
    /// a check's finding count changes. See [`widgets::reselect`].
    pub selected: Option<String>,
    /// What was last rendered. Rendering builds it and hit-testing reads it, so
    /// the two cannot disagree about which row is where.
    rows: Vec<Row>,
}

/// One check's verdict and its findings.
pub struct Group {
    check: &'static str,
    /// The one-line verdict: "none", "3 finding(s)", or why it could not run.
    headline: String,
    level: Level,
    findings: Vec<Finding>,
}

struct Finding {
    text: String,
    /// The path this finding is about, if it names one. Enter opens it.
    path: Option<String>,
}

/// The three states a check can be in, kept visually distinct exactly as the
/// CLI keeps them: `ok: true, warn: true` means "it passed, but you should
/// know", never a softer failure.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Level {
    Pass,
    Warn,
    Fail,
}

impl Level {
    fn symbol(self) -> &'static str {
        match self {
            Level::Fail => "✗",
            Level::Warn => "!",
            Level::Pass => "✓",
        }
    }

    fn style(self) -> Style {
        match self {
            Level::Fail => Style::default().fg(Color::Red),
            Level::Warn => Style::default().fg(Color::Yellow),
            Level::Pass => Style::default().fg(Color::Green),
        }
    }
}

struct Row {
    key: String,
    text: String,
    style: Style,
    /// What Enter opens from this row, if anything.
    target: Option<View>,
}

// ---------------------------------------------------------------- the contract

pub fn refresh(app: &mut App) {
    refresh_setup_if_due(app);
    refresh_behaviour(app);
}

/// A finding that names a path opens that path. Nothing else here is an entity.
pub fn on_enter(app: &App) -> Option<View> {
    app.health
        .list
        .selected()
        .and_then(|i| app.health.rows.get(i))
        .and_then(|row| row.target.clone())
}

pub fn handle_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Char('s') => app.health.show_all_setup = !app.health.show_all_setup,
        KeyCode::Char('d') => run_setup(app),
        KeyCode::Char('g') => run_deep(app),
        _ => return false,
    }
    true
}

pub fn row_at(app: &App, _x: u16, y: u16) -> Option<usize> {
    widgets::row_at(app.content_area, y, 1, app.health.list.offset())
        .filter(|i| *i < app.health.rows.len())
}

pub fn select(app: &mut App, index: usize) {
    select_index(app, Some(index));
}

/// `g` leads: it is the only key here that costs anything, and the only one
/// that can make the ui pause.
pub fn help() -> &'static str {
    "g: git checks  d: re-run setup  s: all setup checks  j/k: move"
}

// ------------------------------------------------------------------ the badge

/// The Health tab's label, indicator included — `Health`, `Health !` or
/// `Health ✗`. Rendered by `root_labels` in mod.rs, exactly as the Messages
/// unread badge is, so `widgets::tab_rects` keeps hit-testing the label that
/// was actually drawn.
///
/// The whole job the old Doctor tab was doing was making a failing check
/// impossible to miss, and demoting it must not lose that. Pure, cheap and
/// subprocess-free: the behaviour half comes off the read model mod.rs has
/// already refreshed this tick; the setup half is whatever
/// [`refresh_setup_if_due`] last found.
pub fn tab_label(app: &App) -> String {
    match badge(app) {
        Some(level) => format!("Health {}", level.symbol()),
        None => View::Health.label(),
    }
}

fn badge(app: &App) -> Option<Level> {
    // Off the per-tick cache: live from any screen, at no cost.
    let storming = !app.data.storms().is_empty();
    let behaving = app
        .health
        .behaviour
        .iter()
        .chain(app.health.deep.iter())
        .map(|g| g.level)
        .max()
        .unwrap_or(Level::Pass);
    let setup = match &app.health.report {
        Some(r) if !r.healthy => Level::Fail,
        Some(r) if r.checks.iter().any(|c| c.warn) => Level::Warn,
        _ => Level::Pass,
    };
    let worst = if storming {
        Level::Fail
    } else {
        behaving.max(setup)
    };
    (worst != Level::Pass).then_some(worst)
}

// ------------------------------------------------------------------- the work

/// Run the setup checks if their own clock says so, from whatever screen the
/// operator is on.
///
/// Called once per tick by mod.rs — the same hook and the same reasoning as
/// `messages::refresh_unread_if_due`: the header indicator has to be current
/// from ANY screen (a failing check nobody can see is the defect this bead was
/// filed for), and doctor spawns `bd` twice, so it gets a clock of its own
/// instead of the 1 Hz one. Two spawns a minute against a loop that wakes every
/// second — three orders of magnitude away from the trap this repo hit when the
/// dashboard spawned `bd` ~10x/second. It rides the 1 s wake that already
/// happens, so it adds no event-loop wakeups.
pub fn refresh_setup_if_due(app: &mut App) {
    let due = app
        .health
        .last_setup
        .is_none_or(|at| at.elapsed() >= SETUP_INTERVAL);
    if due {
        run_setup(app);
    }
}

fn run_setup(app: &mut App) {
    app.health.report = Some(doctor::checks(&app.repo_root));
    app.health.last_setup = Some(Instant::now());
}

fn refresh_behaviour(app: &mut App) {
    let events = app.data.events().len();
    if app.health.behaviour_events == Some(events) {
        return;
    }
    app.health.behaviour_events = Some(events);
    app.health.behaviour = behaviour_groups(&app.repo_root, app.data.storms());
}

/// The offline checks. Every one of these reads `.pact/` (and, for
/// `claim-lease-divergence`, the committed Beads export) and nothing else — no
/// subprocess, so they are safe on the tick that a changed event log triggers.
fn behaviour_groups(repo_root: &Path, storms: &[RetryStorm]) -> Vec<Group> {
    vec![
        check_group(repo_root, Check::StaleHolds, "stale-holds", |r| {
            r.stale_holds
                .iter()
                .map(|h| Finding {
                    text: format!(
                        "{} — {} held it {} and never renewed",
                        h.path,
                        h.agent,
                        human_secs(h.held_secs.unwrap_or(0))
                    ),
                    path: Some(h.path.clone()),
                })
                .collect()
        }),
        check_group(repo_root, Check::DoubleWin, "double-win", |r| {
            r.double_wins
                .iter()
                .map(|d| Finding {
                    text: format!(
                        "{} — {} took it while {} still held it",
                        d.path,
                        d.incoming_agent,
                        d.already_holding
                            .iter()
                            .map(|h| h.agent.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    path: Some(d.path.clone()),
                })
                .collect()
        }),
        check_group(
            repo_root,
            Check::SilentContention,
            "silent-contention",
            |r| {
                r.silent_contentions
                    .iter()
                    .map(|s| Finding {
                        text: format!(
                            "{} — {} was refused, {} released it without a word",
                            s.path, s.refused_agent, s.holder
                        ),
                        path: Some(s.path.clone()),
                    })
                    .collect()
            },
        ),
        check_group(
            repo_root,
            Check::ClaimLeaseDivergence,
            "claim-lease-divergence",
            |r| {
                r.claim_divergences
                    .iter()
                    .map(|c| Finding {
                        text: format!(
                            "{} — {} held it under {}, last assigned to {}",
                            c.path, c.agent, c.bead, c.assignee
                        ),
                        path: Some(c.path.clone()),
                    })
                    .collect()
            },
        ),
        storm_group(storms),
    ]
}

/// `retry-storm`, off the read model rather than re-run: `Store::storms()` IS
/// `pact audit --check retry-storm`'s verdict, already computed this tick.
fn storm_group(storms: &[RetryStorm]) -> Group {
    let findings = storms
        .iter()
        .map(|s| Finding {
            text: format!(
                "{} — {} retried {} times{}",
                s.path,
                s.agent,
                s.refusals,
                match (s.median_gap_secs, s.median_holder_remaining_secs) {
                    (Some(gap), Some(remaining)) => format!(
                        ", every {} against {} of hold left",
                        human_secs(gap),
                        human_secs(remaining)
                    ),
                    _ => String::new(),
                }
            ),
            path: Some(s.path.clone()),
        })
        .collect();
    group("retry-storm", findings, None)
}

/// The git-backed checks, on the keypress that says so.
fn run_deep(app: &mut App) {
    let started = Instant::now();
    let groups = vec![
        check_group(
            &app.repo_root,
            Check::CommitCorrelation,
            "commit-correlation",
            |r| {
                // Worst first: committing where nobody held the path risks your
                // own work, committing where somebody ELSE held it corrupts
                // theirs. `holds_with_no_commit` is deliberately absent — a
                // read-only lease closes exactly like that, and audit does not
                // count it as a finding either.
                let cross = r.cross_held_commits.iter().map(|c| Finding {
                    text: format!(
                        "{} — {} committed {} while {} held it",
                        c.path, c.committer_agent, c.hash, c.holder
                    ),
                    path: Some(c.path.clone()),
                });
                let uncovered = r.uncovered_commits.iter().map(|c| Finding {
                    text: format!("{} — {} by {} under no lease", c.path, c.hash, c.author),
                    path: Some(c.path.clone()),
                });
                let concurrent = r.concurrent_writes.iter().map(|w| Finding {
                    text: format!(
                        "{} — {} and {} held it at once, {} commit(s) inside",
                        w.path,
                        w.first_agent,
                        w.second_agent,
                        w.commits_in_overlap.len()
                    ),
                    path: Some(w.path.clone()),
                });
                cross.chain(uncovered).chain(concurrent).collect()
            },
        ),
        check_group(
            &app.repo_root,
            Check::MergeDivergence,
            "merge-divergence",
            |r| {
                r.merge_divergences
                    .iter()
                    .map(|d| Finding {
                        text: format!(
                            "{} — {} started from content {} never left behind",
                            d.path, d.acquired_by, d.released_by
                        ),
                        path: Some(d.path.clone()),
                    })
                    .collect()
            },
        ),
    ];
    app.health.deep_note = Some(format!(
        "read git history in {:.1}s — press g to run again",
        started.elapsed().as_secs_f64()
    ));
    app.health.deep = groups;
}

fn check_group(
    repo_root: &Path,
    check: Check,
    name: &'static str,
    findings: impl Fn(&CheckReport) -> Vec<Finding>,
) -> Group {
    match audit::run_check(repo_root, check, None, false) {
        Ok(report) => {
            // "could not run" is not "nothing found", and audit is careful to
            // keep the two apart — so is this.
            let unavailable = report
                .git_unavailable
                .clone()
                .or_else(|| report.claim_unavailable.clone());
            group(name, findings(&report), unavailable)
        }
        Err(e) => group(name, Vec::new(), Some(format!("{e:#}"))),
    }
}

fn group(check: &'static str, findings: Vec<Finding>, unavailable: Option<String>) -> Group {
    let (level, headline) = match (unavailable, findings.len()) {
        (Some(reason), _) => (Level::Warn, format!("could not run — {reason}")),
        (None, 0) => (Level::Pass, "none".to_string()),
        (None, 1) => (Level::Fail, "1 finding".to_string()),
        (None, n) => (Level::Fail, format!("{n} findings")),
    };
    Group {
        check,
        headline,
        level,
        findings,
    }
}

// ------------------------------------------------------------------- the rows

fn section(title: &str, subtitle: &str) -> Row {
    Row {
        key: format!("section:{title}"),
        text: format!("{title} — {subtitle}"),
        style: Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        target: None,
    }
}

fn note(key: &str, text: &str) -> Row {
    Row {
        key: format!("note:{key}"),
        text: format!("  {text}"),
        style: Style::default().fg(Color::DarkGray),
        target: None,
    }
}

fn check_row(key: String, level: Level, name: &str, detail: &str) -> Row {
    Row {
        key,
        text: format!("  {} {name}: {detail}", level.symbol()),
        style: level.style(),
        target: None,
    }
}

fn build_rows(app: &App) -> Vec<Row> {
    let mut rows = vec![section("SETUP", "the repository, not the run")];
    match &app.health.report {
        None => rows.push(note("setup", "press r to run the setup checks (spawns bd)")),
        Some(report) => rows.extend(setup_rows(report, app.health.show_all_setup)),
    }

    rows.push(section(
        "BEHAVIOUR",
        &format!("this run, over {} events", app.data.events().len()),
    ));
    if app.health.behaviour.is_empty() {
        rows.push(note("behaviour", "press r to run the fleet checks"));
    }
    rows.extend(group_rows("beh", &app.health.behaviour));

    rows.push(section("DEEP", "reads git history, so not on the timer"));
    if app.health.deep.is_empty() {
        rows.push(note(
            "deep",
            "not run — press g (the ui pauses while git history is read)",
        ));
    }
    rows.extend(group_rows("deep", &app.health.deep));
    if let Some(n) = &app.health.deep_note {
        rows.push(note("deep-ran", n));
    }
    rows
}

/// Green collapses to the verdict alone; anything else expands to the checks
/// that have something to say. `s` shows the rest.
fn setup_rows(report: &doctor::DoctorReport, show_all: bool) -> Vec<Row> {
    let noisy: Vec<&doctor::DoctorCheck> =
        report.checks.iter().filter(|c| !c.ok || c.warn).collect();
    let level = match (report.healthy, noisy.is_empty()) {
        (false, _) => Level::Fail,
        (true, false) => Level::Warn,
        (true, true) => Level::Pass,
    };

    // The same sentence the CLI prints, count included. Rendering the verdict
    // from `healthy` alone showed "all checks passed" above a visible `!`.
    let mut rows = vec![check_row(
        "setup:verdict".to_string(),
        level,
        "setup",
        &format!(
            "{} ({} check{})",
            doctor::summary(report),
            report.checks.len(),
            if report.checks.len() == 1 { "" } else { "s" }
        ),
    )];

    let shown: Vec<&doctor::DoctorCheck> = if show_all {
        report.checks.iter().collect()
    } else {
        noisy
    };
    rows.extend(shown.into_iter().map(|c| {
        let level = match (c.ok, c.warn) {
            (false, _) => Level::Fail,
            (true, true) => Level::Warn,
            (true, false) => Level::Pass,
        };
        check_row(format!("setup:{}", c.name), level, c.name, &c.detail)
    }));
    rows.push(note(
        "setup-toggle",
        if show_all {
            "s: hide the passing setup checks"
        } else {
            "s: show every setup check"
        },
    ));
    rows
}

fn group_rows(prefix: &str, groups: &[Group]) -> Vec<Row> {
    let mut rows = Vec::new();
    for g in groups {
        rows.push(check_row(
            format!("{prefix}:{}", g.check),
            g.level,
            g.check,
            &g.headline,
        ));
        rows.extend(g.findings.iter().enumerate().map(|(i, f)| Row {
            key: format!("{prefix}:{}:{i}", g.check),
            text: format!("      {}", f.text),
            style: Style::default(),
            target: f.path.clone().map(View::Path),
        }));
    }
    rows
}

// ---------------------------------------------------------------- the drawing

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    // Built here and hit-tested from here: `row_at` reads exactly the rows this
    // just drew, which is `tab_rects`' discipline applied to a list.
    app.health.rows = build_rows(app);
    let index = {
        let keys: Vec<&str> = app.health.rows.iter().map(|r| r.key.as_str()).collect();
        widgets::reselect(
            &keys,
            app.health.selected.as_deref(),
            app.health.list.selected(),
        )
    };
    select_index(app, index);

    let selected = app.health.list.selected();
    let items: Vec<ListItem> = app
        .health
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let style = if widgets::is_hovered_not_selected(app.hovered_row, selected, i) {
                row.style.patch(widgets::hover_style())
            } else {
                row.style
            };
            ListItem::new(Line::from(Span::styled(row.text.clone(), style)))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" health "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    // The real ListState, not a clone: the offset the widget scrolls to is what
    // `row_at` hit-tests against, and a discarded clone leaves it at 0 forever.
    frame.render_stateful_widget(list, area, &mut app.health.list);
}

fn select_index(app: &mut App, index: Option<usize>) {
    app.health.list.select(index);
    app.health.selected = index
        .and_then(|i| app.health.rows.get(i))
        .map(|r| r.key.clone());
}

fn move_selection(app: &mut App, delta: isize) {
    let index = widgets::step(app.health.rows.len(), app.health.list.selected(), delta);
    select_index(app, index);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use std::time::Instant;

    fn check(name: &'static str, ok: bool, warn: bool) -> doctor::DoctorCheck {
        doctor::DoctorCheck {
            name,
            ok,
            warn,
            detail: format!("{name} detail"),
        }
    }

    /// An App with nothing behind it: `repo_root` is a scratch dir, so no check
    /// that runs by accident can read the developer's own `.pact/`.
    fn app_at(root: PathBuf) -> App {
        App {
            repo_root: root,
            agent: Some("operator".to_string()),
            nav: super::super::nav::Nav::default(),
            data: super::super::data::Store::default(),
            fleet: super::super::fleet::State::default(),
            activity: super::super::activity::State::default(),
            messages: super::super::messages::State::default(),
            detail: super::super::detail::State::default(),
            health: State::default(),
            status: None,
            last_refresh: Instant::now(),
            header_area: Rect::new(0, 0, 90, 1),
            content_area: Rect::new(0, 3, 90, 20),
            hovered_tab: None,
            hovered_row: None,
        }
    }

    fn app() -> App {
        app_at(PathBuf::new())
    }

    fn rendered(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                app.content_area = area;
                render(frame, area, app)
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// The demotion itself: 25 green checks are one line, and a red one is not.
    #[test]
    fn a_green_setup_is_one_line_and_a_failing_one_expands_itself() {
        let mut app = app();
        app.health.report = Some(doctor::DoctorReport {
            healthy: true,
            checks: (0..3).map(|_| check("gitignore", true, false)).collect(),
        });
        let rows = build_rows(&app);
        let setup: Vec<&Row> = rows
            .iter()
            .filter(|r| r.key.starts_with("setup:"))
            .collect();
        assert_eq!(setup.len(), 1, "three green checks are one verdict line");
        assert!(setup[0].text.contains("all checks passed"));

        // One failing check and the screen opens itself — no keypress.
        app.health.report = Some(doctor::DoctorReport {
            healthy: false,
            checks: vec![
                check("gitignore", true, false),
                check("Beads CLI", false, false),
            ],
        });
        let text = rendered(&mut app);
        assert!(text.contains("some checks failed"), "{text}");
        assert!(text.contains("Beads CLI"), "{text}");
        assert!(
            !text.contains("gitignore"),
            "a passing check stays collapsed: {text}"
        );

        // ...and `s` brings the passing ones back.
        handle_key(&mut app, KeyCode::Char('s'));
        assert!(rendered(&mut app).contains("gitignore"));
    }

    /// `ok: true, warn: true` is a louder pass, not a softer failure, and the
    /// CLI keeps the three apart with `✗ ! ✓`. So does this.
    #[test]
    fn the_three_check_states_stay_visually_distinct() {
        let mut app = app();
        app.health.report = Some(doctor::DoctorReport {
            healthy: true,
            checks: vec![
                check("protocol files", true, true),
                check("gitignore", true, false),
            ],
        });
        app.health.show_all_setup = true;
        let text = rendered(&mut app);
        assert!(text.contains("! protocol files"), "{text}");
        assert!(text.contains("✓ gitignore"), "{text}");
        assert_eq!(badge(&app), Some(Level::Warn), "a warning still shows");

        app.health.report.as_mut().unwrap().healthy = false;
        assert!(rendered(&mut app).contains("✗ setup"));
        assert_eq!(badge(&app), Some(Level::Fail));
    }

    /// The indicator that has to survive the demotion: a failing check is
    /// visible from another screen, without opening Health.
    #[test]
    fn the_header_badge_is_the_worst_of_setup_and_behaviour() {
        let mut app = app();
        assert_eq!(
            tab_label(&app),
            "Health",
            "nothing known yet, nothing shouted"
        );

        app.health.report = Some(doctor::DoctorReport {
            healthy: true,
            checks: vec![check("gitignore", true, false)],
        });
        assert_eq!(tab_label(&app), "Health");

        // A fleet-behaviour finding alone raises it — that is the half this
        // dashboard never showed at all.
        app.health.behaviour = vec![group(
            "stale-holds",
            vec![Finding {
                text: "src/a.rs — a held it 2h and never renewed".to_string(),
                path: Some("src/a.rs".to_string()),
            }],
            None,
        )];
        assert_eq!(tab_label(&app), "Health ✗");

        // A check that could not run is a warning, never a silent pass.
        app.health.behaviour = vec![group(
            "claim-lease-divergence",
            Vec::new(),
            Some("no beads export".into()),
        )];
        assert_eq!(tab_label(&app), "Health !");

        // And it is what the header actually renders, so a `✗` widens the tab
        // it belongs to rather than being invented twice.
        assert_eq!(super::super::root_labels(&app)[3], "Health !");
    }

    /// A finding names a path; Enter opens that path. Enter mutates nothing —
    /// `on_enter` takes `&App`, so that is a compile error rather than a review.
    #[test]
    fn enter_on_a_finding_opens_the_path_it_names() {
        let mut app = app();
        app.health.behaviour = vec![group(
            "silent-contention",
            vec![Finding {
                text: "src/lease.rs — b was refused".to_string(),
                path: Some("src/lease.rs".to_string()),
            }],
            None,
        )];
        rendered(&mut app);

        let finding = app
            .health
            .rows
            .iter()
            .position(|r| r.key == "beh:silent-contention:0")
            .unwrap();
        select(&mut app, finding);
        assert_eq!(on_enter(&app), Some(View::Path("src/lease.rs".into())));

        // The headline above it is a verdict, not an entity.
        select(&mut app, finding - 1);
        assert_eq!(on_enter(&app), None);
    }

    /// A click and a hover ask the same `row_at`, and it reads the rows
    /// rendering just built.
    #[test]
    fn a_click_lands_on_the_row_it_hit() {
        let mut app = app();
        rendered(&mut app);
        let area = Rect::new(0, 0, 120, 24);
        app.content_area = area;

        assert_eq!(row_at(&app, 0, area.y), None, "the border is not a row");
        assert_eq!(row_at(&app, 0, area.y + 1), Some(0));
        assert_eq!(row_at(&app, 0, area.y + 3), Some(2));
        // Past the last row is a no-op, not a bogus selection.
        assert_eq!(row_at(&app, 0, area.y + 200), None);
    }

    /// The trap this bead exists to avoid: a git subprocess on a 1 Hz loop.
    #[test]
    fn the_git_backed_checks_run_on_a_keypress_and_never_on_the_timer() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path().to_path_buf());

        refresh(&mut app);
        refresh(&mut app);
        assert!(
            app.health.deep.is_empty(),
            "the refresh timer must never reach git"
        );
        assert!(rendered(&mut app).contains("press g"));

        // The offline half did run, and reports rather than pretends: an empty
        // scratch repo has no findings.
        assert_eq!(app.health.behaviour.len(), 5);
        assert!(app
            .health
            .behaviour
            .iter()
            .all(|g| g.findings.is_empty() && g.level != Level::Fail));

        handle_key(&mut app, KeyCode::Char('g'));
        assert_eq!(app.health.deep.len(), 2);
        assert!(rendered(&mut app).contains("read git history in"));
    }

    /// Doctor spawns `bd` twice, so it must never sit on the 1 Hz tick — the
    /// dashboard once spawned `bd` ~10x/second for less. It gets its own clock
    /// instead, which is what makes the header indicator current from a screen
    /// that is not this one.
    #[test]
    fn the_setup_checks_have_their_own_clock_and_it_is_far_from_the_frame_clock() {
        assert!(SETUP_INTERVAL >= Duration::from_secs(30));

        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path().to_path_buf());

        // Nothing has run yet, so the first tick from any screen runs them:
        // an operator who never opens Health still sees a failing check.
        refresh_setup_if_due(&mut app);
        assert!(!app.health.report.as_ref().unwrap().checks.is_empty());

        // Marked, so a re-run is visible: `checks` comes back populated from
        // every real run, and only a run would refill it.
        app.health.report.as_mut().unwrap().checks.clear();
        refresh(&mut app);
        refresh_setup_if_due(&mut app);
        assert!(
            app.health.report.as_ref().unwrap().checks.is_empty(),
            "a tick inside the interval must not re-run them"
        );

        // `d` re-runs them on demand, and so does the clock coming due. A
        // scratch dir is not a repo, so they come back unhealthy.
        handle_key(&mut app, KeyCode::Char('d'));
        assert!(!app.health.report.as_ref().unwrap().checks.is_empty());
        assert!(!app.health.report.as_ref().unwrap().healthy);

        app.health.report.as_mut().unwrap().checks.clear();
        app.health.last_setup = Some(Instant::now() - SETUP_INTERVAL);
        refresh_setup_if_due(&mut app);
        assert!(!app.health.report.as_ref().unwrap().checks.is_empty());
    }

    /// The read model parses the event log once per tick; asking audit the same
    /// question again on every tick would put that parse back.
    #[test]
    fn the_behaviour_checks_are_recomputed_only_when_the_log_moves() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path().to_path_buf());
        refresh_behaviour(&mut app);
        assert_eq!(app.health.behaviour_events, Some(0));

        app.health.behaviour.clear();
        refresh_behaviour(&mut app);
        assert!(
            app.health.behaviour.is_empty(),
            "an unchanged log recomputes nothing"
        );
    }

    /// A rebuilt list must not slide the operator's cursor onto another row —
    /// the same defect `widgets::reselect` exists for, on this screen's list.
    #[test]
    fn a_new_finding_does_not_move_the_selection() {
        let mut app = app();
        app.health.behaviour = vec![group("double-win", Vec::new(), None)];
        rendered(&mut app);
        let index = app
            .health
            .rows
            .iter()
            .position(|r| r.key == "beh:double-win")
            .unwrap();
        select(&mut app, index);

        // A stale-holds finding appears above it and pushes every later row down.
        app.health.behaviour.insert(
            0,
            group(
                "stale-holds",
                vec![Finding {
                    text: "src/a.rs".to_string(),
                    path: Some("src/a.rs".to_string()),
                }],
                None,
            ),
        );
        rendered(&mut app);
        assert_eq!(app.health.selected.as_deref(), Some("beh:double-win"));
        assert_ne!(app.health.list.selected(), Some(index), "a different row");
    }
}
