//! The read model: each store under `.pact/` parsed at most once per refresh
//! tick, and the projections every view derives from it.
//!
//! Owned by pact-pyt.11. `mod.rs` calls [`Store::refresh`] once per tick,
//! before any view renders, and views read projections off `App::data` — so a
//! screen is a filter over this, never a second parse of the same file.
//!
//! Two rules this module inherits from the TUI and cannot break: it must not
//! print (a stderr write smears the alternate screen, and `agents::list()`
//! warns on partial failure), and it must not mutate (`lease::peek`, never
//! `lease::list`).
//!
//! **How the no-print rule is kept.** Not by silencing `output::warn` — there
//! is no way to — but by not calling anything that reaches it. Two APIs the
//! epic named are therefore deliberately *not* used here:
//!
//! * `agents::list()` warns on a partial failure, and re-parses BOTH stores on
//!   every call (`events::actors` + `msg::all_messages`), which is precisely
//!   the cost this module exists to remove. [`Store::roster`] rebuilds the same
//!   [`AgentInfo`] rows — the shared type, so `pact agents` and the dashboard
//!   cannot disagree about their shape — from the already-parsed stores plus
//!   `lease::peek`, and the diagnostics it would have printed are returned by
//!   [`Store::diagnostics`] for the status line.
//! * `msg::read_thread` warns, and marks read. [`Store::thread`] is a filter
//!   over the cached messages, so a drill-in never changes delivery state.
//!   (`msg::peek_thread`, the non-marking twin the epic pointed at, is
//!   `#[cfg(feature = "mcp")]` and does not exist in a `--features ui` build.)
//!
//! **What "at most once per tick" means exactly.** Each store is stamped by
//! `(mtime, len)`; an unchanged tick re-reads NOTHING, however many views ask.
//! On a tick where the event log did change it is read twice — once raw here,
//! once by `audit::run_check`, which applies annotation exclusion this module
//! deliberately does not reimplement — and `.pact/messages.jsonl` likewise
//! (once fanned out for read state, once raw for the `about` path tags, whose
//! fan-out helpers are private to `msg`).
//! `// ponytail: two reads per invalidation, not per tick. Collapsing them
//! needs a `pub(crate)` entry point in `audit`/`msg` that takes pre-loaded
//! rows; worth it only if a profile says so.`

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use crate::agents::AgentInfo;
use crate::audit::{self, Check, RetryStorm};
use crate::events::{self, Event};
use crate::identity;
use crate::lease::{self, LeaseEntry};
use crate::msg::{self, Message};
use crate::watch::{self, WatchRecord};

/// How long a refusal that named no `holder_remaining_secs` stays readable as a
/// live block, and the grace added to one that did.
///
/// A refusal carries the holder's own statement of how much lease it had left,
/// so the honest bound is *that* number rather than a flat cutoff: the block is
/// live until the hold it named would have ended, plus this grace. Five minutes
/// is the fallback for the logs that predate `holder_remaining_secs` — long
/// enough to cover a refusal-then-release round trip (the measured chain in this
/// repo's own log ran 20 minutes end to end, but its refusal *did* carry a
/// remaining of 2393s, which is the case this constant is not for), short enough
/// that a refusal from an hour ago never renders as somebody currently stuck.
///
/// A parameter of [`Store::waiting_on`], not a private constant: what counts as
/// "still blocked" is a judgement the panel should be able to state on screen,
/// and [`WaitingOn::grace_secs`] carries it back out for exactly that.
pub const DEFAULT_GRACE_SECS: i64 = 300;

/// `(mtime, len)` — what a store looked like when it was last read.
type Stamp = Option<(SystemTime, u64)>;

/// Parsed stores plus their derived projections, cached by (mtime, len).
#[derive(Default)]
pub struct Store {
    /// Every event with its 1-based line number, oldest first. The line number
    /// IS the event id (see `events::numbered`) — stable for a given file, and
    /// what `pact audit` prints, so a row here can be pointed at.
    events: Vec<(usize, Event)>,
    messages: Vec<Message>,
    watches: Vec<WatchRecord>,
    /// Path -> ids of the messages tagged as being about it (`--to-owner-of`).
    /// Kept separately because the fanned-out [`Message`] drops `Record::about`.
    about: BTreeMap<String, BTreeSet<String>>,
    storms: Vec<RetryStorm>,
    leases: Vec<LeaseEntry>,
    roster: Vec<AgentInfo>,

    events_stamp: Stamp,
    messages_stamp: Stamp,
    watches_stamp: Stamp,

    unparseable_events: usize,
    unparseable_messages: usize,
    events_error: Option<String>,
    messages_error: Option<String>,
    lease_error: Option<String>,

    /// How many times the event log has actually been parsed. Not for display:
    /// it is the only way a test can assert the cache does what the whole module
    /// exists to do.
    parses: usize,
}

impl Store {
    /// Re-read whatever changed on disk. Called once per refresh tick from the
    /// event loop, never from a view.
    pub fn refresh(&mut self, repo_root: &Path) {
        let pact = crate::repo::pact_dir_path(repo_root);

        if changed(&mut self.events_stamp, &pact.join("events.jsonl")) {
            self.parses += 1;
            match events::numbered(repo_root) {
                Ok((rows, unparseable)) => {
                    self.events = rows;
                    self.unparseable_events = unparseable;
                    self.events_error = None;
                }
                Err(e) => {
                    self.events.clear();
                    self.events_error = Some(format!("event log unreadable: {e:#}"));
                }
            }
            // Reused rather than re-derived: two implementations of "is this a
            // retry storm" would disagree, and `pact audit --check retry-storm`
            // is the one that is documented and tested.
            self.storms = audit::run_check(repo_root, Check::RetryStorm, None, false)
                .map(|r| r.retry_storms)
                .unwrap_or_default();
        }

        if changed(&mut self.messages_stamp, &pact.join("messages.jsonl")) {
            match msg::all_messages(repo_root) {
                Ok(m) => {
                    self.messages = m;
                    self.messages_error = None;
                }
                Err(e) => {
                    self.messages.clear();
                    self.messages_error = Some(format!("message store unreadable: {e:#}"));
                }
            }
            self.about.clear();
            if let Ok((records, unparseable)) = msg::records(repo_root) {
                self.unparseable_messages = unparseable;
                for r in records {
                    for path in r.about {
                        self.about.entry(path).or_default().insert(r.id.clone());
                    }
                }
            }
        }

        if changed(&mut self.watches_stamp, &pact.join("watches.jsonl")) {
            self.watches = watch::records(repo_root)
                .map(|(rows, _)| rows)
                .unwrap_or_default();
        }

        // Not stamped: a lock file appears and vanishes without either JSONL
        // file changing (a lease that lapsed has no event until somebody
        // collects it), and this is a small directory scan rather than a parse.
        // `peek`, never `list` — asking who holds what must not collect the
        // expired lock an operator opened the dashboard to look at.
        match lease::peek(repo_root, true) {
            Ok(entries) => {
                self.leases = entries;
                self.lease_error = None;
            }
            Err(e) => {
                self.leases.clear();
                self.lease_error = Some(format!("leases unreadable: {e:#}"));
            }
        }

        self.rebuild_roster();
    }

    /// What the status line should say about this read, in place of the
    /// warnings the underlying APIs would have written to stderr. Empty is the
    /// normal case.
    pub fn diagnostics(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.events_error.clone());
        out.extend(self.messages_error.clone());
        out.extend(self.lease_error.clone());
        // A torn final line is normal for an append-only log being written
        // right now, so this is a count, never an error.
        if self.unparseable_events > 0 {
            out.push(format!(
                "{} unreadable line(s) in events.jsonl",
                self.unparseable_events
            ));
        }
        if self.unparseable_messages > 0 {
            out.push(format!(
                "{} unreadable line(s) in messages.jsonl",
                self.unparseable_messages
            ));
        }
        out
    }

    // ------------------------------------------------------------- the stores

    /// Every event, oldest first, each with its line number.
    pub fn events(&self) -> &[(usize, Event)] {
        &self.events
    }

    /// The newest `limit` events, oldest-first so a feed reads top to bottom.
    pub fn feed(&self, limit: usize) -> &[(usize, Event)] {
        &self.events[self.events.len().saturating_sub(limit)..]
    }

    /// Every message in the repo — the fleet's conversation, not one inbox.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Live locks, expired ones included (`expired` says which). The caller
    /// filters; nothing here collects.
    pub fn leases(&self) -> &[LeaseEntry] {
        &self.leases
    }

    /// The roster, most recently active first.
    pub fn roster(&self) -> &[AgentInfo] {
        &self.roster
    }

    pub fn storms(&self) -> &[RetryStorm] {
        &self.storms
    }

    // --------------------------------------------------------------- slices

    pub fn events_by_agent(&self, agent: &str) -> Vec<&(usize, Event)> {
        self.events
            .iter()
            .filter(|(_, e)| e.agent == agent)
            .collect()
    }

    pub fn events_by_path(&self, path: &str) -> Vec<&(usize, Event)> {
        self.events
            .iter()
            .filter(|(_, e)| e.path.as_deref() == Some(path))
            .collect()
    }

    /// Who last actually HELD `path` — the cached form of `events::owner_of`,
    /// down to sharing `events::is_custody`, so a refusal or a subscription
    /// never makes the wrong agent look like the owner.
    pub fn owner_of(&self, path: &str) -> Option<&Event> {
        self.events
            .iter()
            .rev()
            .map(|(_, e)| e)
            .filter(|e| events::is_custody(&e.kind))
            .find(|e| e.path.as_deref() == Some(path))
    }

    /// Messages addressed to `agent`, oldest first.
    pub fn inbox(&self, agent: &str) -> Vec<&Message> {
        self.messages.iter().filter(|m| m.to == agent).collect()
    }

    /// Messages `agent` sent, newest first — the order `pact msg sent` uses,
    /// because the question is "did the last one land".
    pub fn sent(&self, agent: &str) -> Vec<&Message> {
        let mut out: Vec<&Message> = self.messages.iter().filter(|m| m.from == agent).collect();
        out.reverse();
        out
    }

    /// The whole thread `id` belongs to, oldest first, WITHOUT marking anything
    /// read. `id` may be a thread id or any message id within it.
    pub fn thread(&self, id: &str) -> Vec<&Message> {
        let thread = self
            .messages
            .iter()
            .find(|m| m.id == id)
            .map_or(id, |m| m.thread.as_str());
        self.messages
            .iter()
            .filter(|m| m.thread == thread)
            .collect()
    }

    /// Messages tagged as being ABOUT `path` — how a handoff outlives the agent
    /// that made it. `path` is repo-relative, as leases and events spell it.
    pub fn messages_about(&self, path: &str) -> Vec<&Message> {
        let Some(ids) = self.about.get(path) else {
            return Vec::new();
        };
        self.messages
            .iter()
            .filter(|m| ids.contains(&m.id))
            .collect()
    }

    /// Who is subscribed to `path` as of `at`, prefix watches included.
    ///
    /// The registry replayed by `watch::was_subscribed_at`, exactly as
    /// [`Self::waiting_on`] replays it — never a second coverage rule here, for
    /// the reason `watch::covers` is private in the first place: `src/render`
    /// must not match `src/renderer.rs`, and a `starts_with` written twice will
    /// get that wrong in one of the two places.
    ///
    /// Added for pact-pyt.4's path view, which has to answer "who hears about
    /// this release" off the per-tick cache rather than by reading
    /// `.pact/watches.jsonl` itself.
    pub fn subscribers(&self, path: &str, at: DateTime<Utc>) -> Vec<&str> {
        let at = at.to_rfc3339();
        let mut agents: Vec<&str> = self.watches.iter().map(|w| w.agent.as_str()).collect();
        agents.sort_unstable();
        agents.dedup();
        agents.retain(|agent| watch::was_subscribed_at(&self.watches, agent, path, &at));
        agents
    }

    // --------------------------------------------------------- waiting-on

    /// Who is blocked, on what, held by whom — the contention graph the event
    /// log has always carried and no surface rendered.
    ///
    /// One row per `(blocked agent, path)`, built from the `refused` events,
    /// which carry the holder and the holder's own remaining lease. Joined to
    /// the two halves of the chain that says what the blocked agent did next:
    /// the watch registry (did it subscribe, or is it retrying?) and the
    /// `notified` events (did the holder's release reach it?).
    ///
    /// `now` is a parameter so the projection is testable against a fixture log
    /// without a clock; the caller passes `Utc::now()`.
    pub fn waiting_on(&self, now: DateTime<Utc>, grace_secs: i64) -> WaitingOn {
        let mut by_pair: BTreeMap<(&str, &str), Vec<&Event>> = BTreeMap::new();
        for (_, e) in &self.events {
            if e.kind != "refused" {
                continue;
            }
            if let Some(path) = e.path.as_deref() {
                by_pair.entry((e.agent.as_str(), path)).or_default().push(e);
            }
        }

        let now_stamp = now.to_rfc3339();
        let mut blocked: Vec<Blocked> = by_pair
            .into_iter()
            .filter_map(|((agent, path), refusals)| {
                // Oldest-first input, so the last is the one that still stands.
                let last = refusals.last()?;
                let refused_at = parse_at(&last.at)?;
                let waited_secs = (now - refused_at).num_seconds();

                Some(Blocked {
                    agent: agent.to_string(),
                    path: path.to_string(),
                    holder: last.holder.clone(),
                    holder_remaining_secs: last.holder_remaining_secs,
                    waited_secs,
                    refusals: refusals.len(),
                    // The live answer, replayed from the registry rather than
                    // re-deriving prefix coverage: `watch::covers` is private and
                    // getting `src/render` vs `src/renderer.rs` wrong is its
                    // documented trap.
                    subscribed: watch::was_subscribed_at(&self.watches, agent, path, &now_stamp),
                    retry_storm: self
                        .storms
                        .iter()
                        .any(|s| s.agent == agent && s.path == path),
                    claimed_at: self.custody_after(agent, path, refused_at),
                    notified_at: self.notified_after(agent, path, refused_at),
                    // The bound, stated per row: a refusal outlives the hold it
                    // named by `grace_secs` and no longer.
                    stale: waited_secs > last.holder_remaining_secs.unwrap_or(0) + grace_secs,
                })
            })
            .collect();

        // Live blocks first, longest wait at the top: the operator's question is
        // "who is stuck right now, and worst".
        blocked.sort_by_key(|b| (!b.live(), std::cmp::Reverse(b.waited_secs)));
        WaitingOn {
            grace_secs,
            blocked,
        }
    }

    /// Did `agent` end up holding `path` after `after`?
    fn custody_after(&self, agent: &str, path: &str, after: DateTime<Utc>) -> Option<String> {
        self.events
            .iter()
            .map(|(_, e)| e)
            .find(|e| {
                e.agent == agent
                    && e.path.as_deref() == Some(path)
                    && events::is_custody(&e.kind)
                    && parse_at(&e.at).is_some_and(|t| t > after)
            })
            .map(|e| e.at.clone())
    }

    /// Did a release of `path` get delivered to `agent` after `after`?
    fn notified_after(&self, agent: &str, path: &str, after: DateTime<Utc>) -> Option<String> {
        self.events
            .iter()
            .map(|(_, e)| e)
            .find(|e| {
                e.kind == "notified"
                    && e.path.as_deref() == Some(path)
                    && e.subscriber.as_deref() == Some(agent)
                    && parse_at(&e.at).is_some_and(|t| t > after)
            })
            .map(|e| e.at.clone())
    }

    // ----------------------------------------------------------- the roster

    /// The same three sources `agents::list` reads — live locks, lease history,
    /// message traffic — but off the already-parsed stores, and returning its
    /// diagnostics instead of printing them. See the module doc.
    fn rebuild_roster(&mut self) {
        let mut seen: BTreeMap<String, AgentInfo> = BTreeMap::new();
        for entry in &self.leases {
            let info = observe(&mut seen, &entry.lease.agent, &entry.lease.acquired_at);
            info.leases_held += 1;
            // From the LIVE lock only, mirroring `agents::list` — the dashboard's
            // Via column says what an agent is running now, and a lock is the
            // only evidence that survives exactly as long as the process does.
            info.harness = entry.lease.harness.clone().or_else(|| info.harness.take());
            info.model = entry.lease.model.clone().or_else(|| info.model.take());
        }
        for (_, e) in &self.events {
            observe(&mut seen, &e.agent, &e.at).lease_events += 1;
        }
        for m in &self.messages {
            observe(&mut seen, &m.from, &m.created_at).messages_sent += 1;
            observe(&mut seen, &m.to, &m.created_at).messages_received += 1;
        }
        let mut roster: Vec<AgentInfo> = seen.into_values().collect();
        // Most recently active first; name breaks ties so the list is stable.
        roster.sort_by(|a, b| {
            parse_at(&b.last_seen)
                .cmp(&parse_at(&a.last_seen))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.roster = roster;
    }
}

/// One edge of the contention graph: `agent` wants `path`, `holder` has it.
#[derive(Debug, Clone)]
pub struct Blocked {
    pub agent: String,
    pub path: String,
    /// The holder named by the refusal. `None` on logs predating the field.
    pub holder: Option<String>,
    /// What the holder said it had left, at the moment of the refusal.
    pub holder_remaining_secs: Option<i64>,
    pub waited_secs: i64,
    /// How many times this agent has been refused this path, ever.
    pub refusals: usize,
    /// Did it subscribe, per the protocol, instead of polling?
    pub subscribed: bool,
    /// Does `pact audit --check retry-storm` call this a poll loop?
    pub retry_storm: bool,
    /// When the blocked agent got the path — the block resolved by winning.
    pub claimed_at: Option<String>,
    /// When the holder's release was delivered to it — the block resolved by
    /// the watch working.
    pub notified_at: Option<String>,
    /// Older than the hold it named plus the caller's grace: history, not a
    /// live block.
    pub stale: bool,
}

impl Blocked {
    /// Is this agent still waiting, as far as the log can tell?
    pub fn live(&self) -> bool {
        !self.stale && self.claimed_at.is_none() && self.notified_at.is_none()
    }
}

/// The waiting-on graph, with the staleness bound it was computed under so a
/// renderer can say what "waiting" meant.
#[derive(Debug, Clone)]
pub struct WaitingOn {
    pub grace_secs: i64,
    /// Live blocks first, longest wait first.
    pub blocked: Vec<Blocked>,
}

impl WaitingOn {
    pub fn live(&self) -> impl Iterator<Item = &Blocked> {
        self.blocked.iter().filter(|b| b.live())
    }
}

/// Has `path` changed since it was last read? Updates the stamp as a side
/// effect. A missing file stamps as `None`, so "never existed" and "deleted"
/// are both handled without a special case.
///
/// `len` is half the key on purpose: both stores are append-only, so a write
/// within the same mtime tick still changes the length. A rewrite that happened
/// to land on the same mtime AND the same length would be missed; the log's
/// trim rewrites it to a different size, so that shape does not occur.
fn changed(stamp: &mut Stamp, path: &Path) -> bool {
    let current = std::fs::metadata(path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())));
    if *stamp == current {
        return false;
    }
    *stamp = current;
    true
}

fn parse_at(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Record that `name` was seen acting at `at`, keeping the most recent stamp.
fn observe<'a>(
    seen: &'a mut BTreeMap<String, AgentInfo>,
    name: &str,
    at: &str,
) -> &'a mut AgentInfo {
    let info = seen.entry(name.to_string()).or_insert_with(|| AgentInfo {
        name: name.to_string(),
        last_seen: String::new(),
        leases_held: 0,
        lease_events: 0,
        messages_sent: 0,
        messages_received: 0,
        // A name that cannot pass `identity::validate` is one no pact process
        // could have run as, however much traffic the store shows for it.
        name_valid: identity::validate(name).is_ok(),
        harness: None,
        model: None,
    });
    if parse_at(at) > parse_at(&info.last_seen) {
        info.last_seen = at.to_string();
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Fixture logs, written as raw JSONL — the same bytes pact appends, so the
    /// parse path is under test too, and every timestamp is chosen rather than
    /// "now".
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".pact")).unwrap();
        tmp
    }

    fn store_file(root: &Path, name: &str) -> PathBuf {
        root.join(".pact").join(name)
    }

    fn write(root: &Path, name: &str, lines: &[serde_json::Value]) {
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(store_file(root, name), body).unwrap();
    }

    /// The chain this repo's own log recorded, verbatim (see pact-pyt.11):
    /// docs-story is refused docs/tui.md, subscribes 17s later, and 20 minutes
    /// on the release reaches it as a notice.
    fn refusal(
        at: &str,
        agent: &str,
        path: &str,
        holder: &str,
        remaining: i64,
    ) -> serde_json::Value {
        serde_json::json!({
            "at": at, "agent": agent, "kind": "refused", "path": path, "detail": null,
            "holder": holder, "holder_remaining_secs": remaining,
        })
    }

    fn event(at: &str, agent: &str, kind: &str, path: &str) -> serde_json::Value {
        serde_json::json!({
            "at": at, "agent": agent, "kind": kind, "path": path, "detail": null,
        })
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-14T14:40:00+00:00")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn contention_log() -> Vec<serde_json::Value> {
        vec![
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
        ]
    }

    fn watch_line(at: &str, agent: &str, path: &str) -> serde_json::Value {
        serde_json::json!({ "at": at, "agent": agent, "kind": "watch", "path": path })
    }

    // ------------------------------------------------------------- the cache

    #[test]
    fn a_missing_store_is_an_empty_read_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::default();
        store.refresh(tmp.path());

        assert!(store.events().is_empty());
        assert!(store.messages().is_empty());
        assert!(store.diagnostics().is_empty());
        assert!(
            !tmp.path().join(".pact").exists(),
            "a pure read created .pact/"
        );
    }

    #[test]
    fn an_unchanged_store_is_not_reparsed_however_many_ticks_ask() {
        let tmp = repo();
        write(tmp.path(), "events.jsonl", &contention_log());

        let mut store = Store::default();
        store.refresh(tmp.path());
        assert_eq!(store.parses, 1);
        assert_eq!(store.events().len(), 3);

        store.refresh(tmp.path());
        store.refresh(tmp.path());
        assert_eq!(store.parses, 1, "an unchanged log was parsed again");
    }

    #[test]
    fn the_cache_invalidates_on_a_change_not_on_a_timer() {
        let tmp = repo();
        write(tmp.path(), "events.jsonl", &contention_log());

        let mut store = Store::default();
        store.refresh(tmp.path());

        let mut grown = contention_log();
        grown.push(event(
            "2026-08-14T14:58:02+00:00",
            "orchestrator",
            "released",
            "docs/tui.md",
        ));
        write(tmp.path(), "events.jsonl", &grown);

        store.refresh(tmp.path());
        assert_eq!(store.parses, 2);
        assert_eq!(store.events().len(), 4);
    }

    // -------------------------------------------------------- waiting-on

    #[test]
    fn the_refuse_subscribe_notify_chain_is_reproduced() {
        let tmp = repo();
        write(tmp.path(), "events.jsonl", &contention_log());
        write(
            tmp.path(),
            "watches.jsonl",
            &[watch_line(
                "2026-08-14T14:37:42+00:00",
                "docs-story",
                "docs/tui.md",
            )],
        );

        let mut store = Store::default();
        store.refresh(tmp.path());
        let waiting = store.waiting_on(now(), DEFAULT_GRACE_SECS);

        let b = waiting.blocked.first().expect("one blocked agent");
        assert_eq!(b.agent, "docs-story");
        assert_eq!(b.path, "docs/tui.md");
        assert_eq!(b.holder.as_deref(), Some("orchestrator"));
        assert_eq!(b.holder_remaining_secs, Some(2393));
        assert_eq!(b.waited_secs, 155);
        assert!(b.subscribed, "the watch registry says it subscribed");
        assert!(!b.retry_storm, "one refusal is contention, not a storm");
        assert!(b.live(), "the holder still has 2393s of lease left");
        assert_eq!(waiting.grace_secs, DEFAULT_GRACE_SECS);
    }

    #[test]
    fn a_notified_release_closes_the_block() {
        let tmp = repo();
        let mut log = contention_log();
        log.push(serde_json::json!({
            "at": "2026-08-14T14:58:02+00:00", "agent": "orchestrator", "kind": "notified",
            "path": "docs/tui.md", "detail": null,
            "subscriber": "docs-story", "message_id": "pact-msg-3b28",
        }));
        write(tmp.path(), "events.jsonl", &log);

        let mut store = Store::default();
        store.refresh(tmp.path());
        // After the notice landed.
        let after = DateTime::parse_from_rfc3339("2026-08-14T15:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let waiting = store.waiting_on(after, DEFAULT_GRACE_SECS);

        let b = waiting.blocked.first().unwrap();
        assert_eq!(b.notified_at.as_deref(), Some("2026-08-14T14:58:02+00:00"));
        assert!(!b.live(), "the release reached it; it is not still waiting");
        assert_eq!(waiting.live().count(), 0);
    }

    #[test]
    fn winning_the_path_closes_the_block_too() {
        let tmp = repo();
        let mut log = contention_log();
        log.push(event(
            "2026-08-14T14:39:00+00:00",
            "docs-story",
            "acquired",
            "docs/tui.md",
        ));
        write(tmp.path(), "events.jsonl", &log);

        let mut store = Store::default();
        store.refresh(tmp.path());
        let b = store.waiting_on(now(), DEFAULT_GRACE_SECS);
        assert!(b.blocked[0].claimed_at.is_some());
        assert!(!b.blocked[0].live());
    }

    #[test]
    fn an_old_refusal_is_never_rendered_as_a_live_block() {
        let tmp = repo();
        write(tmp.path(), "events.jsonl", &contention_log());

        let mut store = Store::default();
        store.refresh(tmp.path());

        // An hour past the 2393s the holder said it had left, plus the grace.
        let much_later = DateTime::parse_from_rfc3339("2026-08-14T16:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let waiting = store.waiting_on(much_later, DEFAULT_GRACE_SECS);
        assert!(waiting.blocked[0].stale);
        assert_eq!(waiting.live().count(), 0);

        // And the bound is the caller's, not a silent constant.
        let generous = store.waiting_on(much_later, 60 * 60 * 24);
        assert!(generous.blocked[0].live());
        assert_eq!(generous.grace_secs, 60 * 60 * 24);
    }

    #[test]
    fn a_refusal_with_no_recorded_remaining_falls_back_to_the_grace() {
        let tmp = repo();
        write(
            tmp.path(),
            "events.jsonl",
            &[serde_json::json!({
                "at": "2026-08-14T14:30:00+00:00", "agent": "docs-story",
                "kind": "refused", "path": "docs/tui.md", "detail": null,
            })],
        );

        let mut store = Store::default();
        store.refresh(tmp.path());
        // 600s after the refusal, with a 300s grace and nothing said about the hold.
        assert!(store.waiting_on(now(), DEFAULT_GRACE_SECS).blocked[0].stale);
        assert!(store.waiting_on(now(), 900).blocked[0].live());
    }

    #[test]
    fn repeated_refusals_collapse_to_one_edge_and_count() {
        let tmp = repo();
        let mut log = contention_log();
        for i in 0..3 {
            log.push(refusal(
                &format!("2026-08-14T14:38:{:02}+00:00", i * 15),
                "docs-story",
                "docs/tui.md",
                "orchestrator",
                2393,
            ));
        }
        write(tmp.path(), "events.jsonl", &log);

        let mut store = Store::default();
        store.refresh(tmp.path());
        let waiting = store.waiting_on(now(), DEFAULT_GRACE_SECS);
        assert_eq!(waiting.blocked.len(), 1, "one edge per (agent, path)");
        assert_eq!(waiting.blocked[0].refusals, 4);
        // The newest refusal is the one that stands: the test clock is 14:40:00
        // and the newest of the four refusals is at 14:38:30, so any older one
        // winning would show a larger wait than 90s.
        assert_eq!(waiting.blocked[0].waited_secs, 90);
    }

    // ------------------------------------------------------------ projections

    #[test]
    fn the_roster_counts_leases_events_and_mail_without_printing() {
        let tmp = repo();
        write(tmp.path(), "events.jsonl", &contention_log());

        let mut store = Store::default();
        store.refresh(tmp.path());

        let names: Vec<&str> = store.roster().iter().map(|a| a.name.as_str()).collect();
        // docs-story acted at 14:37:42, orchestrator at 14:30 — recency order.
        assert_eq!(names, vec!["docs-story", "orchestrator"]);
        let docs = &store.roster()[0];
        assert_eq!(docs.lease_events, 2);
        assert_eq!(docs.leases_held, 0);
        assert!(docs.name_valid);
        assert!(store.diagnostics().is_empty());
    }

    #[test]
    fn slices_are_filters_over_the_one_parse() {
        let tmp = repo();
        let mut log = contention_log();
        log.push(event(
            "2026-08-14T14:39:00+00:00",
            "orchestrator",
            "acquired",
            "src/audit.rs",
        ));
        write(tmp.path(), "events.jsonl", &log);

        let mut store = Store::default();
        store.refresh(tmp.path());

        assert_eq!(store.events_by_agent("orchestrator").len(), 2);
        assert_eq!(store.events_by_path("docs/tui.md").len(), 3);
        assert_eq!(store.feed(2).len(), 2);
        assert_eq!(store.parses, 1, "a slice re-parsed the log");
    }

    #[test]
    fn owner_of_ignores_refusals_and_subscriptions() {
        let tmp = repo();
        write(tmp.path(), "events.jsonl", &contention_log());

        let mut store = Store::default();
        store.refresh(tmp.path());
        // docs-story's refusal and watch are the two most recent events on the
        // path; neither is custody.
        let owner = store.owner_of("docs/tui.md").expect("a holder");
        assert_eq!(owner.agent, "orchestrator");
        assert_eq!(owner.kind, "acquired");
    }

    #[test]
    fn a_torn_line_is_a_diagnostic_not_a_failure() {
        let tmp = repo();
        write(tmp.path(), "events.jsonl", &contention_log());
        let mut body = fs::read_to_string(store_file(tmp.path(), "events.jsonl")).unwrap();
        body.push_str("{\"at\":\"2026-08-14T14:39:0");
        fs::write(store_file(tmp.path(), "events.jsonl"), body).unwrap();

        let mut store = Store::default();
        store.refresh(tmp.path());
        assert_eq!(store.events().len(), 3);
        assert_eq!(store.diagnostics().len(), 1);
        assert!(store.diagnostics()[0].contains("events.jsonl"));
    }
}
