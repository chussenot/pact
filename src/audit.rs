//! Offline analysis of pact's own coordination history.
//!
//! ## Why this exists
//!
//! pact records every acquire, renew, release, steal and expiry in
//! `.pact/events.jsonl`, and until now nothing read it back except `pact log`,
//! which prints the tail. Questions a fleet actually raises — was one path
//! contended by six agents, did anyone hold a lease for an hour without
//! renewing, did two agents ever hold one path at once — needed a human with
//! `jq` and a hypothesis.
//!
//! The last of those is the one that matters, because it has a written trigger
//! condition attached. The guard-file backlog item (**pact-ehi**) says: implement
//! the guard file *if and only if* a double-win appears in a real events log.
//! That is a falsifiable claim with no detector, which makes it a claim nobody
//! can act on. [`Check::DoubleWin`] is the detector, and the two point at each
//! other — the bead names this command, this command's help names the bead.
//!
//! ## Scope, which is narrow on purpose
//!
//! Audit reads **`.pact/` and nothing else**. It never opens `.beads/`, a Dolt
//! directory, a SQLite file or a JSONL export, because "pact never touches the
//! Beads store directly, only the CLI" is an invariant the whole messaging design
//! rests on, and an analytics command is exactly where it would be convenient to
//! break it. Beads-side questions live in `scripts/beads-retro.sh`, which is
//! best-effort, jq-based, and says so in its header.
//!
//! No new dependencies: line-by-line `serde_json` over the log, tolerant of
//! unknown event kinds (a `kind` is a `String`, so a future one parses) and of a
//! truncated final line (an append-only log gets cut mid-write, which is expected
//! rather than corrupt — the count is reported).
//!
//! ## Exit codes
//!
//! `0` clean, `1` findings — the documented contract, reused rather than
//! extended. An audit finding is not a usage error, so it must not be 5, and it
//! is not a lease conflict, so it must not be 2.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::events::{ChainMismatch, Event};
use crate::identity;
use crate::lease::{ttl_as_i64, DEFAULT_TTL_SECS};

/// The default TTL before pact recorded it per-event, used for holds whose
/// opening event carries no `ttl_secs`.
///
/// NOT `DEFAULT_TTL_SECS`. Judging a historical hold against today's compiled
/// default is how raising that default silently rewrites the past: every hold in
/// this repository's log was taken under a 900s TTL and none exceeds 36m, so under
/// a 45m default all 22 findings would vanish without anything having changed
/// about them. A hold is compared against the TTL that was actually in force.
const LEGACY_DEFAULT_TTL_SECS: u64 = 900;

/// The fallback only means anything while the compiled default has moved past it:
/// if the two are equal again, `holds_with_no_recorded_ttl_use_the_legacy_default`
/// silently stops testing anything. A `const` assertion rather than one inside the
/// test, because comparing two constants at run time is a clippy lint and this is
/// knowable at compile time anyway.
const _: () = assert!(crate::lease::DEFAULT_TTL_SECS > LEGACY_DEFAULT_TTL_SECS);

/// How many contended paths and agents a summary lists before it stops being a
/// summary. The full data is in the log; this is the part a human reads.
const TOP_N: usize = 10;

/// The one event kind that is not history: a correction pointing at lines that
/// are.
///
/// `.pact/events.jsonl` is append-only, and that is load-bearing rather than
/// incidental — it is committed, and the guard-file bead (pact-ehi) treats it as
/// the evidence base for a real decision. So a wrong entry is never edited or
/// deleted; it is *annotated*, by appending a record that names the lines and says
/// why. The original stays readable, the correction is attributable, and anyone
/// can disagree with an annotation by reading what it covers.
///
/// Older pact binaries need no change to cope: `kind` is a `String`, so an
/// annotation parses as an unknown kind, opens no hold window and closes none.
/// They simply do not apply the exclusion — which is the safe direction, because
/// it over-reports rather than hiding events.
///
/// The first one exists because on 2026-07-31 hand-run expiry and atomicity
/// experiments in this repository's root wrote six synthetic events: agents
/// `victim`, `ghost` and `grabber` on paths `shared.rs`, `ghost.rs` and `new.rs`,
/// none of which have ever existed here.
pub const ANNOTATION_KIND: &str = "annotation";

/// Which named check to run. Absent means the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    DoubleWin,
    StaleHolds,
    /// pact-m7j.2.5: does every chain-tracked line's `chain_hash` match what
    /// it should be, given the line before it? Separate from the other two
    /// checks on purpose — this one is about the log's own physical
    /// integrity, not about lease behaviour, and a line with no `chain_hash`
    /// is not a finding here (see `Event::chain_hash`'s doc comment).
    ChainIntegrity,
}

impl Check {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "double-win" => Ok(Check::DoubleWin),
            "stale-holds" => Ok(Check::StaleHolds),
            "chain-integrity" => Ok(Check::ChainIntegrity),
            other => Err(anyhow::anyhow!(
                "unknown check \"{other}\"; expected double-win, stale-holds or chain-integrity"
            )),
        }
    }
}

/// One completed or still-open hold of one path by one agent.
#[derive(Debug, Clone, Serialize)]
pub struct Hold {
    pub path: String,
    pub agent: String,
    /// Line number in `.pact/events.jsonl` of the acquire/steal that opened it.
    pub opened_line: usize,
    pub opened_at: String,
    /// `None` while the log shows the lease still held.
    pub closed_line: Option<usize>,
    pub closed_at: Option<String>,
    /// `released`, `force-released` or `expired`.
    pub closed_by: Option<String>,
    /// How many `renewed` events fell inside this window. A long hold that
    /// renewed is following the protocol; one that did not is the smell.
    pub renewals: usize,
    pub held_secs: Option<i64>,
    /// The TTL this hold was taken under, from the opening event.
    pub ttl_secs: u64,
    /// True when the opening event recorded no TTL and
    /// [`LEGACY_DEFAULT_TTL_SECS`] was assumed. Surfaced so a reader can tell a
    /// measured threshold from an inferred one.
    pub ttl_assumed: bool,
}

/// Two agents holding one path at the same time.
#[derive(Debug, Clone, Serialize)]
pub struct DoubleWin {
    pub path: String,
    /// The acquire/steal that should not have succeeded.
    pub incoming_agent: String,
    pub incoming_kind: String,
    pub incoming_line: usize,
    pub incoming_at: String,
    /// Whoever the log says was already holding it, and since when.
    pub already_holding: Vec<HoldingAgent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HoldingAgent {
    pub agent: String,
    pub since: String,
    pub since_line: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contended {
    pub path: String,
    pub holds: usize,
    pub distinct_agents: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentActivity {
    pub agent: String,
    pub events: usize,
    pub holds: usize,
    pub steals: usize,
    pub held_secs_total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HoldStats {
    pub completed: usize,
    pub median_secs: i64,
    pub p90_secs: i64,
    pub max_secs: i64,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub events: usize,
    /// Events dropped because an annotation covers their line. Reported so the
    /// exclusion is itself visible: a statistic that quietly omits data is a
    /// statistic nobody can check.
    pub excluded_by_annotation: usize,
    pub annotations: Vec<Annotation>,
    /// Lines the parser could not read. A torn final line is normal for an
    /// append-only log; a large number here means something else is wrong.
    pub unparseable_lines: usize,
    /// A close-kind event (`released`/`force-released`/`expired`) with no
    /// matching open entry — mirrors `excluded_by_annotation`'s shape for the
    /// same reason: `reconstruct` used to drop such an event with no Hold, no
    /// counter and no trace, which let `by_kind`'s raw count of close events
    /// silently disagree with how many Holds actually closed. This module's
    /// philosophy is to never synthesize a best-effort Hold for history it
    /// cannot actually reconstruct, so this is a count of "something didn't
    /// add up", not a guess at what did.
    pub orphaned_closes: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub agents: Vec<String>,
    pub first_event_at: Option<String>,
    pub last_event_at: Option<String>,
    pub steals: usize,
    pub open_holds: usize,
    pub hold_secs: Option<HoldStats>,
    pub top_contended: Vec<Contended>,
    pub per_agent: Vec<AgentActivity>,
}

/// The report for a named check: findings plus enough context to judge them.
#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub check: &'static str,
    pub events_scanned: usize,
    /// Events an annotation excluded. A check that silently skipped data would be
    /// the same defect as a statistic that did.
    pub excluded_by_annotation: usize,
    pub unparseable_lines: usize,
    /// See `Summary::orphaned_closes` — same meaning, computed by the same
    /// `reconstruct` pass this check also runs.
    pub orphaned_closes: usize,
    pub double_wins: Vec<DoubleWin>,
    pub stale_holds: Vec<Hold>,
    /// `Check::ChainIntegrity` only: lines whose `chain_hash` did not match
    /// what it should be, given the line before it.
    pub chain_breaks: Vec<ChainMismatch>,
    /// `Check::ChainIntegrity` only: how many lines carried a `chain_hash` at
    /// all. Not a finding either way — context for `chain_untracked`.
    pub chain_tracked: usize,
    /// `Check::ChainIntegrity` only: lines with no `chain_hash` — predating
    /// chain tracking, or not written by pact. Reported, never flagged: see
    /// `Event::chain_hash`'s doc comment for why a missing hash must not read
    /// as tampering.
    pub chain_untracked: usize,
    /// The **current** default TTL, for context only — *not* the threshold any
    /// finding was judged against. Each `Hold` carries its own `ttl_secs`, read from
    /// the event, so this field must never be used to re-derive a verdict: it moves
    /// when the default is recalibrated and the findings do not.
    pub ttl_secs: Option<u64>,
}

impl CheckReport {
    pub fn findings(&self) -> usize {
        self.double_wins.len() + self.stale_holds.len() + self.chain_breaks.len()
    }
}

/// `--since`: an RFC3339 instant, or a duration back from now.
///
/// Both spellings because both are what people reach for: an exact instant when
/// correlating with something else, and "the last day" when triaging. Durations
/// are `<n><unit>` with unit in `smhdw` — deliberately not a general parser,
/// because `--since 3` meaning three of something unstated is a bug waiting.
pub fn parse_since(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .with_context(|| format!("--since {s}: expected RFC3339 or a duration like 7d"))?,
    );
    let n: i64 = num
        .parse()
        .with_context(|| format!("--since {s}: {num} is not a number"))?;
    let d = match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        other => {
            return Err(anyhow::anyhow!(
                "--since {s}: unknown unit \"{other}\"; use s, m, h, d or w"
            ))
        }
    };
    Ok(Utc::now() - d)
}

fn parse_at(at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Does this kind open a hold window?
///
/// `stolen` counts, and that is not a detail: a takeover is a hold. Treating it
/// as anything else would make every reclaim invisible to `stale-holds` and would
/// hide exactly the double-wins a `--steal` could cause.
fn opens(kind: &str) -> bool {
    matches!(kind, "acquired" | "stolen")
}

/// Does this kind close one?
///
/// `expired` closes the **previous** holder's window: `lease.rs` logs it under
/// `existing.agent`, not under the agent taking over. That is what makes a
/// routine reclaim — `expired` then `stolen` — read correctly instead of looking
/// like two agents holding at once.
fn closes(kind: &str) -> bool {
    matches!(kind, "released" | "force-released" | "expired")
}

/// Reconstruct every hold window, and notice where two overlap.
///
/// One pass per path over time-ordered events, tracking who is currently open.
/// An acquire arriving while somebody *else* is open is the double-win; an
/// acquire while the *same* agent is open is a re-entrant refresh, which pact
/// does deliberately and which must not be reported.
///
/// The third element is the count of close-kind events (`released` /
/// `force-released` / `expired`) that named an agent+path with no open entry
/// to close. Without a hold to close, such an event otherwise vanishes from
/// the reconstruction with no Hold, no counter and no trace — see
/// `Summary::orphaned_closes`.
fn reconstruct(events: &[(usize, Event)]) -> (Vec<Hold>, Vec<DoubleWin>, usize) {
    let mut by_path: BTreeMap<&str, Vec<(usize, &Event)>> = BTreeMap::new();
    for (line, e) in events {
        if let Some(p) = e.path.as_deref() {
            by_path.entry(p).or_default().push((*line, e));
        }
    }

    let mut holds = Vec::new();
    let mut doubles = Vec::new();
    let mut orphaned_closes = 0;

    for (path, mut rows) in by_path {
        // Stable by time, then by line, so two events in the same millisecond
        // keep the order the log recorded them in.
        rows.sort_by(|a, b| {
            parse_at(&a.1.at)
                .cmp(&parse_at(&b.1.at))
                .then(a.0.cmp(&b.0))
        });

        // agent -> (opened_line, opened_at, renewals, ttl_secs, ttl_assumed)
        let mut open: BTreeMap<String, (usize, String, usize, u64, bool)> = BTreeMap::new();

        for (line, e) in rows {
            if opens(&e.kind) {
                let others: Vec<HoldingAgent> = open
                    .iter()
                    .filter(|(a, _)| *a != &e.agent)
                    .map(|(a, (l, at, ..))| HoldingAgent {
                        agent: a.clone(),
                        since: at.clone(),
                        since_line: *l,
                    })
                    .collect();
                if !others.is_empty() {
                    doubles.push(DoubleWin {
                        path: path.to_string(),
                        incoming_agent: e.agent.clone(),
                        incoming_kind: e.kind.clone(),
                        incoming_line: line,
                        incoming_at: e.at.clone(),
                        already_holding: others,
                    });
                }
                // A re-entrant acquire by the same agent refreshes rather than
                // opening a second window, so the original open time is kept.
                open.entry(e.agent.clone()).or_insert((
                    line,
                    e.at.clone(),
                    0,
                    e.ttl_secs.unwrap_or(LEGACY_DEFAULT_TTL_SECS),
                    e.ttl_secs.is_none(),
                ));
            } else if e.kind == "renewed" {
                if let Some(slot) = open.get_mut(&e.agent) {
                    slot.2 += 1;
                    // A renew can change the TTL, so the window adopts the newest
                    // one it was actually granted.
                    if let Some(ttl) = e.ttl_secs {
                        slot.3 = ttl;
                        slot.4 = false;
                    }
                }
            } else if e.kind == "restored" {
                // pact-m7j.1.3: `acquire_many`'s rollback undid exactly one
                // "renewed" it had logged for this path — the refresh never
                // survived, so it must not go on counting as a renewal that
                // exempts this hold from `stale-holds`. And the TTL back in
                // force is whatever this event carries (the pre-batch one),
                // not the higher one the undone refresh briefly granted.
                if let Some(slot) = open.get_mut(&e.agent) {
                    slot.2 = slot.2.saturating_sub(1);
                    if let Some(ttl) = e.ttl_secs {
                        slot.3 = ttl;
                        slot.4 = false;
                    }
                }
            } else if closes(&e.kind) {
                // A force-released event is filed under the agent who forced
                // it, not the one displaced — the opposite of `expired`,
                // which lease.rs deliberately logs under the dead holder's
                // own name (see the struct doc on `Event`). Without this,
                // `open.remove(&e.agent)` looked for the FORCER's window,
                // found none, and let the real holder's window run on
                // unclosed while counting the close as orphaned instead
                // (pact-m7j.2.6). `displaced` is `None` on every other kind
                // and on a force-release with no surviving holder name, so
                // this falls back to `e.agent` exactly as before there.
                let holder = e.displaced.as_deref().unwrap_or(e.agent.as_str());
                if let Some((oline, oat, renewals, ttl, ttl_assumed)) = open.remove(holder) {
                    let held = parse_at(&oat)
                        .zip(parse_at(&e.at))
                        .map(|(a, b)| (b - a).num_seconds());
                    holds.push(Hold {
                        path: path.to_string(),
                        agent: holder.to_string(),
                        opened_line: oline,
                        opened_at: oat,
                        closed_line: Some(line),
                        closed_at: Some(e.at.clone()),
                        closed_by: Some(e.kind.clone()),
                        renewals,
                        held_secs: held,
                        ttl_secs: ttl,
                        ttl_assumed,
                    });
                } else {
                    // A close with nothing open to close: never guessed at
                    // with a synthetic Hold, only counted.
                    orphaned_closes += 1;
                }
            }
        }

        // Whatever is still open at the end of the log: a live lease, or an agent
        // that exited without releasing. Reported with no close, never guessed at.
        for (agent, (oline, oat, renewals, ttl, ttl_assumed)) in open {
            holds.push(Hold {
                path: path.to_string(),
                agent,
                opened_line: oline,
                opened_at: oat,
                closed_line: None,
                closed_at: None,
                closed_by: None,
                renewals,
                held_secs: None,
                ttl_secs: ttl,
                ttl_assumed,
            });
        }
    }

    (holds, doubles, orphaned_closes)
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// What one pass over the log produced.
struct Loaded {
    events: Vec<(usize, Event)>,
    unparseable: usize,
    /// Events dropped because an annotation covers their line.
    excluded: usize,
    /// The annotations themselves, so a report can say what was excluded and why
    /// rather than only how many.
    annotations: Vec<Annotation>,
}

/// A correction: which lines are not real history, and who says so.
#[derive(Debug, Clone, Serialize)]
pub struct Annotation {
    pub line: usize,
    pub at: String,
    pub actor: Option<String>,
    pub note: Option<String>,
    pub covers_lines: Vec<usize>,
    /// `false` when `actor` is `Some` but fails [`identity::validate`]'s
    /// `[a-z0-9][a-z0-9-]{1,31}` format check. `true` when `actor` is absent —
    /// unattributed is a different, already-surfaced condition ("unknown" in
    /// the rendered report), not a malformed one.
    ///
    /// pact has no command that writes an annotation itself — every one today
    /// is a hand-typed JSONL line — so there is no write-time gate to put this
    /// check behind. Flagging it here, where the line is read back, is the
    /// only reachable point: rejecting the line outright would make a single
    /// bad `actor` field silently swallow the correction it was meant to
    /// record, which is worse than trusting a forgeable field already was.
    pub actor_valid: bool,
}

/// Read the log, drop annotated lines unless asked not to, and narrow by `--since`.
///
/// The exclusion happens BEFORE `--since`, deliberately: an annotation and the
/// lines it covers are usually days apart, so filtering by time first would drop
/// the correction and silently re-admit the events it corrects.
fn load(
    repo_root: &std::path::Path,
    since: Option<DateTime<Utc>>,
    include_annotated: bool,
) -> Result<Loaded> {
    let (all, unparseable) = crate::events::numbered(repo_root)?;

    let mut annotations = Vec::new();
    let mut covered: BTreeSet<usize> = BTreeSet::new();
    for (line, e) in &all {
        if e.kind != ANNOTATION_KIND {
            continue;
        }
        let covers = e.covers_lines.clone().unwrap_or_default();
        covered.extend(covers.iter().copied());
        annotations.push(Annotation {
            line: *line,
            at: e.at.clone(),
            actor: e.actor.clone(),
            note: e.detail.clone(),
            covers_lines: covers,
            actor_valid: e
                .actor
                .as_deref()
                .is_none_or(|a| identity::validate(a).is_ok()),
        });
    }

    let mut excluded = 0;
    let events: Vec<(usize, Event)> = all
        .into_iter()
        .filter(|(line, e)| {
            // Annotation rows are never history themselves, whatever
            // `--include-annotated` says: counting them as events would inflate
            // every total with records that describe the log rather than the fleet.
            if e.kind == ANNOTATION_KIND {
                return false;
            }
            if !include_annotated && covered.contains(line) {
                excluded += 1;
                return false;
            }
            true
        })
        .filter(|(_, e)| match since {
            None => true,
            Some(cut) => parse_at(&e.at).map(|t| t >= cut).unwrap_or(false),
        })
        .collect();

    Ok(Loaded {
        events,
        unparseable,
        excluded,
        annotations,
    })
}

pub fn summary(
    repo_root: &std::path::Path,
    since: Option<DateTime<Utc>>,
    include_annotated: bool,
) -> Result<Summary> {
    let loaded = load(repo_root, since, include_annotated)?;
    let unparseable = loaded.unparseable;
    let events = loaded.events;
    let (holds, _, orphaned_closes) = reconstruct(&events);

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut agents: BTreeSet<String> = BTreeSet::new();
    let mut per: BTreeMap<String, AgentActivity> = BTreeMap::new();
    for (_, e) in &events {
        *by_kind.entry(e.kind.clone()).or_insert(0) += 1;
        agents.insert(e.agent.clone());
        let a = per.entry(e.agent.clone()).or_insert_with(|| AgentActivity {
            agent: e.agent.clone(),
            events: 0,
            holds: 0,
            steals: 0,
            held_secs_total: 0,
        });
        a.events += 1;
        if e.kind == "stolen" {
            a.steals += 1;
        }
    }
    for h in &holds {
        if let Some(a) = per.get_mut(&h.agent) {
            a.holds += 1;
            a.held_secs_total += h.held_secs.unwrap_or(0);
        }
    }

    let mut per_agent: Vec<AgentActivity> = per.into_values().collect();
    per_agent.sort_by(|a, b| b.events.cmp(&a.events).then(a.agent.cmp(&b.agent)));
    per_agent.truncate(TOP_N);

    // Contention is distinct agents first, then hold count: a path one agent took
    // forty times is busy, a path four agents took once each is contended, and
    // the second is the one worth a human's attention.
    let mut per_path: BTreeMap<&str, (usize, BTreeSet<&str>)> = BTreeMap::new();
    for h in &holds {
        let e = per_path.entry(&h.path).or_default();
        e.0 += 1;
        e.1.insert(&h.agent);
    }
    let mut top_contended: Vec<Contended> = per_path
        .into_iter()
        .map(|(p, (n, set))| Contended {
            path: p.to_string(),
            holds: n,
            distinct_agents: set.len(),
        })
        .collect();
    top_contended.sort_by(|a, b| {
        b.distinct_agents
            .cmp(&a.distinct_agents)
            .then(b.holds.cmp(&a.holds))
            .then(a.path.cmp(&b.path))
    });
    top_contended.truncate(TOP_N);

    let mut durations: Vec<i64> = holds.iter().filter_map(|h| h.held_secs).collect();
    durations.sort_unstable();
    let hold_secs = (!durations.is_empty()).then(|| HoldStats {
        completed: durations.len(),
        median_secs: percentile(&durations, 0.5),
        p90_secs: percentile(&durations, 0.9),
        max_secs: *durations.last().unwrap_or(&0),
    });

    Ok(Summary {
        events: events.len(),
        excluded_by_annotation: loaded.excluded,
        annotations: loaded.annotations,
        unparseable_lines: unparseable,
        orphaned_closes,
        steals: by_kind.get("stolen").copied().unwrap_or(0),
        by_kind,
        agents: agents.into_iter().collect(),
        first_event_at: events.first().map(|(_, e)| e.at.clone()),
        last_event_at: events.last().map(|(_, e)| e.at.clone()),
        open_holds: holds.iter().filter(|h| h.closed_at.is_none()).count(),
        hold_secs,
        top_contended,
        per_agent,
    })
}

pub fn run_check(
    repo_root: &std::path::Path,
    check: Check,
    since: Option<DateTime<Utc>>,
    include_annotated: bool,
) -> Result<CheckReport> {
    let loaded = load(repo_root, since, include_annotated)?;
    let unparseable = loaded.unparseable;
    let events = loaded.events;
    let (holds, doubles, orphaned_closes) = reconstruct(&events);

    let mut report = CheckReport {
        check: match check {
            Check::DoubleWin => "double-win",
            Check::StaleHolds => "stale-holds",
            Check::ChainIntegrity => "chain-integrity",
        },
        events_scanned: events.len(),
        excluded_by_annotation: loaded.excluded,
        unparseable_lines: unparseable,
        orphaned_closes,
        double_wins: Vec::new(),
        stale_holds: Vec::new(),
        chain_breaks: Vec::new(),
        chain_tracked: 0,
        chain_untracked: 0,
        ttl_secs: None,
    };

    match check {
        Check::DoubleWin => report.double_wins = doubles,
        Check::StaleHolds => {
            // No single threshold any more: each hold is judged against its own
            // recorded TTL. `ttl_secs` on the report stays as the CURRENT default,
            // for context only, and the per-hold value is what decided each row.
            report.ttl_secs = Some(DEFAULT_TTL_SECS);
            // Over TTL AND never renewed. The protocol says a long task must not
            // outlive its lease and to renew if it does, so a long hold that
            // renewed is an agent following instructions — reporting it would
            // train people to ignore this check. A hold that lapsed into
            // `expired` is included whatever its length: that is the same smell,
            // already realised.
            report.stale_holds = holds
                .into_iter()
                .filter(|h| {
                    let over = h.held_secs.unwrap_or(0) > ttl_as_i64(h.ttl_secs);
                    let lapsed = h.closed_by.as_deref() == Some("expired");
                    (over || lapsed) && h.renewals == 0
                })
                .collect();
            // Longest first: the worst offender is what a reader wants at the top.
            // `Reverse` rather than a flipped comparator, which clippy is right to
            // object to — the key form cannot get the operands the wrong way round.
            report
                .stale_holds
                .sort_by_key(|h| std::cmp::Reverse(h.held_secs));
        }
        Check::ChainIntegrity => {
            // The chain is a property of PHYSICAL line adjacency in the raw
            // file, not of `load`'s annotation-filtered, `--since`-narrowed
            // view: an annotation line and anything it covers are still real
            // entries the writer's hash chain ran through. Reads the log a
            // second time rather than reusing `events` above for exactly that
            // reason — `--since`/`--include-annotated` apply to every other
            // check but must not apply to this one.
            let (raw, _) = crate::events::numbered(repo_root)?;
            let (mismatches, tracked, untracked) = crate::events::verify_chain(&raw);
            report.events_scanned = raw.len();
            // These two describe the lease-hold reconstruction this check
            // does not perform; zeroed rather than left showing the filtered
            // view's numbers, which would describe a scan this check never ran.
            report.excluded_by_annotation = 0;
            report.orphaned_closes = 0;
            report.chain_breaks = mismatches;
            report.chain_tracked = tracked;
            report.chain_untracked = untracked;
        }
    }
    Ok(report)
}

// --------------------------------------------------------------- rendering

fn secs(n: i64) -> String {
    crate::lease::human_secs(n)
}

pub fn render_summary(s: &Summary) -> String {
    if s.events == 0 {
        return format!(
            "no coordination history yet{}\n\n\
             .pact/events.jsonl is written by the lease commands; run one and it appears. \
             If this repository HAS been used, the log may predate events-log preservation \
             — see docs/audit.md.",
            if s.unparseable_lines > 0 {
                format!(" ({} unreadable line(s))", s.unparseable_lines)
            } else {
                String::new()
            }
        );
    }

    let mut out = Vec::new();
    out.push(format!(
        "{} events from {} agent(s)",
        s.events,
        s.agents.len()
    ));
    if let (Some(a), Some(b)) = (&s.first_event_at, &s.last_event_at) {
        out.push(format!("  span   {a}  ->  {b}"));
    }
    if s.unparseable_lines > 0 {
        out.push(format!(
            "  note   {} unreadable line(s) — a torn final line is normal for an append-only log",
            s.unparseable_lines
        ));
    }
    if s.orphaned_closes > 0 {
        out.push(format!(
            "  note   {} close event(s) with no matching open — not counted as a Hold",
            s.orphaned_closes
        ));
    }
    // Never silent. A statistic that omits data without saying so is a statistic
    // nobody can check, and the whole reason annotations exist is that the log is
    // evidence.
    if s.excluded_by_annotation > 0 {
        out.push(format!(
            "  note   {} event(s) excluded by annotation (--include-annotated to see them)",
            s.excluded_by_annotation
        ));
        for a in &s.annotations {
            out.push(format!(
                "         line {} by {}{}: {}",
                a.line,
                a.actor.as_deref().unwrap_or("unknown"),
                if a.actor_valid {
                    ""
                } else {
                    " [INVALID ACTOR — does not match [a-z0-9][a-z0-9-]{1,31}]"
                },
                a.note.as_deref().unwrap_or("(no note)")
            ));
        }
    }

    let kinds: Vec<String> = s.by_kind.iter().map(|(k, n)| format!("{k} {n}")).collect();
    out.push(format!("  kinds  {}", kinds.join(", ")));
    if s.open_holds > 0 {
        out.push(format!("  open   {} lease(s) still held", s.open_holds));
    }

    if let Some(h) = &s.hold_secs {
        out.push(String::new());
        out.push(format!(
            "hold time over {} completed hold(s): median {}, p90 {}, max {}",
            h.completed,
            secs(h.median_secs),
            secs(h.p90_secs),
            secs(h.max_secs)
        ));
    }

    if !s.top_contended.is_empty() {
        out.push(String::new());
        out.push("most contended paths".to_string());
        for c in &s.top_contended {
            out.push(format!(
                "  {:<44} {} hold(s) by {} agent(s)",
                c.path, c.holds, c.distinct_agents
            ));
        }
    }

    if !s.per_agent.is_empty() {
        out.push(String::new());
        out.push("busiest agents".to_string());
        for a in &s.per_agent {
            out.push(format!(
                "  {:<24} {} event(s), {} hold(s){}, {} held",
                a.agent,
                a.events,
                a.holds,
                if a.steals > 0 {
                    format!(", {} steal(s)", a.steals)
                } else {
                    String::new()
                },
                secs(a.held_secs_total)
            ));
        }
    }

    out.join("\n")
}

pub fn render_check(r: &CheckReport) -> String {
    let mut out = vec![format!(
        "{}: scanned {} event(s)",
        r.check, r.events_scanned
    )];
    if r.unparseable_lines > 0 {
        out.push(format!("  {} unreadable line(s)", r.unparseable_lines));
    }
    if r.orphaned_closes > 0 {
        out.push(format!(
            "  {} close event(s) with no matching open — not counted as a Hold",
            r.orphaned_closes
        ));
    }
    if r.excluded_by_annotation > 0 {
        out.push(format!(
            "  {} event(s) excluded by annotation — this check did not look at them",
            r.excluded_by_annotation
        ));
    }

    if r.check == "chain-integrity" {
        // Informational regardless of findings: a reader needs to know how
        // much of the log this check could even speak to before it says
        // whether that portion is intact — see `Event::chain_hash`'s doc
        // comment on why an untracked line is not itself a finding.
        out.push(format!(
            "  {} line(s) chain-tracked, {} line(s) predate chain tracking or were not written \
             by pact",
            r.chain_tracked, r.chain_untracked
        ));
    }

    if r.findings() == 0 {
        out.push(match r.check {
            "double-win" => {
                "no overlapping hold windows — no two agents ever held one path at once".to_string()
            }
            "chain-integrity" => {
                "every chain-tracked line matches the line before it — no gap, edit or forgery \
                 detected in the tracked portion of the log"
                    .to_string()
            }
            _ => "no holds ran past their own recorded TTL without a renew".to_string(),
        });
        return out.join("\n");
    }

    for d in &r.double_wins {
        out.push(String::new());
        out.push(format!("DOUBLE-WIN on {}", d.path));
        out.push(format!(
            "  {} {} at {} (line {})",
            d.incoming_agent, d.incoming_kind, d.incoming_at, d.incoming_line
        ));
        for h in &d.already_holding {
            out.push(format!(
                "  while {} had held it since {} (line {})",
                h.agent, h.since, h.since_line
            ));
        }
    }
    if !r.double_wins.is_empty() {
        out.push(String::new());
        // The whole reason this check exists, said where the reader is looking.
        out.push(
            "This is the trigger condition for the guard-file backlog item (pact-ehi), which\n\
             says to implement the guard file if and only if a double-win appears in a real\n\
             events log. Attach this output to that bead: it is the evidence the bead is\n\
             waiting for, and the reason not to implement on suspicion."
                .to_string(),
        );
    }

    for h in &r.stale_holds {
        let ended = match h.closed_by.as_deref() {
            Some(k) => format!("ended by {k}"),
            None => "still open".to_string(),
        };
        out.push(format!(
            "  {:<40} {:<16} held {:>8} vs ttl {:>7}{}, {} (line {})",
            h.path,
            h.agent,
            h.held_secs.map(secs).unwrap_or_else(|| "?".to_string()),
            secs(ttl_as_i64(h.ttl_secs)),
            if h.ttl_assumed { "*" } else { " " },
            ended,
            h.opened_line
        ));
    }
    if !r.stale_holds.is_empty() {
        out.push(String::new());
        // Deliberately no "distinct incidents" count. One `lease acquire` naming
        // three paths writes three event rows whose timestamps differ by
        // microseconds, so grouping them would need a tolerance window — and a
        // number that depends on an arbitrary tolerance is worse than no number.
        // Holds sharing an agent and a duration are almost certainly one acquire;
        // the reader can see that from the rows.
        let assumed = r.stale_holds.iter().filter(|h| h.ttl_assumed).count();
        if assumed > 0 {
            out.push(format!(
                "  * {assumed} hold(s) predate pact recording a TTL per event; judged against the \
                 {}s default of that era, not today's.",
                LEGACY_DEFAULT_TTL_SECS
            ));
        }
        out.push(String::new());
        out.push(format!(
            "{} hold(s) ran past their OWN recorded TTL without a single renew. Rows sharing an agent and a\n\
             duration are one `lease acquire` that named several paths. The protocol says a long\n\
             task must not outlive its lease, and `pact lease renew` refreshes it — a lapsed lease\n\
             is reclaimable by anyone, so each of these is a window where a peer could have taken\n\
             a path its holder still believed it owned. (The current default is {}.)",
            r.stale_holds.len(),
            secs(ttl_as_i64(r.ttl_secs.unwrap_or(DEFAULT_TTL_SECS)))
        ));
    }

    for m in &r.chain_breaks {
        out.push(String::new());
        out.push(format!("CHAIN BREAK at line {}", m.line));
        out.push(format!("  {} {} at {}", m.agent, m.kind, m.at));
        out.push(format!(
            "  expected chain_hash {}, found {}",
            m.expected, m.found
        ));
    }
    if !r.chain_breaks.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} line(s) whose chain_hash does not match the line before it — a hand-edited or\n\
             forged line, or the file was altered outside pact. This is about the log's own\n\
             physical integrity and is unrelated to {} line(s) elsewhere that simply predate\n\
             chain tracking or were not written by pact; those are not evidence of tampering by\n\
             themselves.",
            r.chain_breaks.len(),
            r.chain_untracked
        ));
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a log and audit it. Takes raw lines so a test can plant a truncated
    /// one, an unknown kind, or outright junk.
    fn with_log(lines: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let pact = tmp.path().join(".pact");
        std::fs::create_dir_all(&pact).unwrap();
        std::fs::write(pact.join("events.jsonl"), lines.join("\n")).unwrap();
        tmp
    }

    fn ev(at: &str, agent: &str, kind: &str, path: &str) -> String {
        format!(r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}"}}"#)
    }

    #[test]
    fn an_empty_log_is_not_an_error() {
        let tmp = with_log(&[]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.events, 0);
        assert!(render_summary(&s).contains("no coordination history"));

        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0, "an empty log has no findings, not one");
    }

    /// No log at all — a repo that has never used pact. Must read as empty rather
    /// than failing, and must NOT create the file: audit is a question.
    #[test]
    fn a_missing_log_reads_as_empty_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        assert_eq!(summary(tmp.path(), None, false).unwrap().events, 0);
        assert!(
            !tmp.path().join(".pact").exists(),
            "auditing must not create .pact/"
        );
    }

    #[test]
    fn a_clean_history_has_no_findings() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:00Z", "agent-a", "released", "src/a.rs"),
            &ev("2026-08-01T10:06:00Z", "agent-b", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:07:00Z", "agent-b", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0, "sequential holds are not a double-win");

        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.events, 4);
        assert_eq!(s.agents, ["agent-a", "agent-b"]);
        assert_eq!(s.open_holds, 0);
        let h = s.hold_secs.expect("two completed holds");
        assert_eq!(h.completed, 2);
        assert_eq!(h.max_secs, 300);
        assert_eq!(s.top_contended[0].distinct_agents, 2);
    }

    #[test]
    fn one_double_win_is_found_with_both_agents_and_lines() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            // No release in between: agent-b takes a path agent-a still holds.
            &ev("2026-08-01T10:01:00Z", "agent-b", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-b", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        let d = &r.double_wins[0];
        assert_eq!(d.path, "src/a.rs");
        assert_eq!(d.incoming_agent, "agent-b");
        assert_eq!(d.incoming_line, 2, "the line number is the event id");
        assert_eq!(d.already_holding.len(), 1);
        assert_eq!(d.already_holding[0].agent, "agent-a");
        assert_eq!(d.already_holding[0].since_line, 1);

        // The rendering must point at the bead whose trigger condition this is.
        let text = render_check(&r);
        assert!(text.contains("pact-ehi"), "{text}");
        assert!(
            text.contains("agent-a") && text.contains("agent-b"),
            "{text}"
        );
    }

    /// A reclaim is `expired` (the dead holder) then `stolen` (the new one), and
    /// it must NOT read as a double-win — otherwise every routine takeover in
    /// every log is a false finding and the check is useless.
    #[test]
    fn a_routine_reclaim_after_expiry_is_not_a_double_win() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:30:00Z", "agent-a", "expired", "src/a.rs"),
            &ev("2026-08-01T10:30:01Z", "agent-b", "stolen", "src/a.rs"),
            &ev("2026-08-01T10:31:00Z", "agent-b", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0, "expired closes the old window first");
    }

    /// A `--steal` over a LIVE lease has no `expired` before it, so it is a real
    /// overlap and must be reported. That asymmetry is the whole reason `expired`
    /// rows exist.
    #[test]
    fn stealing_a_live_lease_is_a_double_win() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:30Z", "agent-b", "stolen", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        assert_eq!(r.double_wins[0].incoming_kind, "stolen");
    }

    /// pact re-acquires its own lease to refresh it, deliberately. That is one
    /// window, not two, and not a finding.
    #[test]
    fn a_reentrant_acquire_by_the_same_agent_is_not_a_double_win() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:10Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:20Z", "agent-a", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(
            s.hold_secs.unwrap().completed,
            1,
            "a refresh extends one hold rather than opening a second"
        );
    }

    /// An append-only log gets cut mid-write. The torn line must be counted and
    /// skipped, and everything before it must still be analysed.
    #[test]
    fn a_truncated_final_line_is_counted_not_fatal() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:01:00Z", "agent-a", "released", "src/a.rs"),
            r#"{"at":"2026-08-01T10:02:00Z","agent":"agent-b","kind":"acq"#,
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.events, 2, "the two whole events still count");
        assert_eq!(s.unparseable_lines, 1);
        assert!(render_summary(&s).contains("unreadable"));
        // And a check still runs rather than refusing on a torn tail.
        assert_eq!(
            run_check(tmp.path(), Check::DoubleWin, None, false)
                .unwrap()
                .unparseable_lines,
            1
        );
    }

    /// A kind this version has never heard of must pass through. The log is
    /// append-only and older binaries read newer logs.
    #[test]
    fn an_unknown_kind_is_counted_and_ignored_by_the_checks() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:30Z", "agent-a", "teleported", "src/a.rs"),
            &ev("2026-08-01T10:01:00Z", "agent-a", "released", "src/a.rs"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.by_kind.get("teleported"), Some(&1));
        assert_eq!(s.unparseable_lines, 0, "unknown is not unparseable");
        // It neither opens nor closes a window.
        assert_eq!(s.hold_secs.unwrap().completed, 1);
        assert_eq!(
            run_check(tmp.path(), Check::DoubleWin, None, false)
                .unwrap()
                .findings(),
            0
        );
    }

    /// pact-juz.1: `refused` (a denied acquire, logged under the requester)
    /// must stay neutral in `reconstruct` — same shape as `renewed`/
    /// `restored` — so a run full of contention that never once succeeded a
    /// steal doesn't skew hold-duration or double-win math for the agent who
    /// actually holds the path.
    #[test]
    fn a_refused_event_neither_opens_nor_closes_a_hold_window() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "holder", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:30Z", "loser", "refused", "src/a.rs"),
            &ev("2026-08-01T10:01:00Z", "holder", "released", "src/a.rs"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.by_kind.get("refused"), Some(&1));
        // The holder's window closes cleanly at 30s, not skewed by the
        // refusal in between — "refused" is neither an open nor a close.
        assert_eq!(s.hold_secs.unwrap().completed, 1);
        assert_eq!(
            run_check(tmp.path(), Check::DoubleWin, None, false)
                .unwrap()
                .findings(),
            0,
            "a refused acquire must never itself read as a double-win"
        );
    }

    #[test]
    fn stale_holds_reports_long_unrenewed_holds_only() {
        let tmp = with_log(&[
            // 2 hours, never renewed: the smell.
            &ev("2026-08-01T10:00:00Z", "slow", "acquired", "src/slow.rs"),
            &ev("2026-08-01T12:00:00Z", "slow", "released", "src/slow.rs"),
            // 2 hours, renewed: following the protocol, not a finding.
            &ev("2026-08-01T10:00:00Z", "good", "acquired", "src/good.rs"),
            &ev("2026-08-01T10:30:00Z", "good", "renewed", "src/good.rs"),
            &ev("2026-08-01T12:00:00Z", "good", "released", "src/good.rs"),
            // Short and unrenewed: fine.
            &ev("2026-08-01T10:00:00Z", "quick", "acquired", "src/quick.rs"),
            &ev("2026-08-01T10:00:30Z", "quick", "released", "src/quick.rs"),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(r.findings(), 1, "only the long unrenewed hold");
        assert_eq!(r.stale_holds[0].agent, "slow");
        assert_eq!(r.stale_holds[0].held_secs, Some(7200));
        assert_eq!(r.ttl_secs, Some(DEFAULT_TTL_SECS));
        assert!(render_check(&r).contains("without a single renew"));
    }

    /// A lease that lapsed is the same smell already realised, whatever its
    /// duration.
    fn ev_ttl(at: &str, agent: &str, kind: &str, path: &str, ttl: u64) -> String {
        format!(
            r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}","ttl_secs":{ttl}}}"#
        )
    }

    /// The assertion that makes raising the default safe. Each hold is judged
    /// against the TTL IT recorded, so a hold taken under a short TTL stays a
    /// finding no matter what the binary is compiled with — and one taken under a
    /// long TTL is not a finding even though it ran longer.
    #[test]
    fn a_hold_is_judged_against_its_own_recorded_ttl_not_the_compiled_default() {
        let tmp = with_log(&[
            // 30 min under a 10 min TTL: over its own, and would ALSO be over a
            // 900s default — so this row alone would not prove anything.
            &ev_ttl("2026-08-01T10:00:00Z", "short", "acquired", "a.rs", 600),
            &ev_ttl("2026-08-01T10:30:00Z", "short", "released", "a.rs", 600),
            // 30 min under a 2 hour TTL: LONGER than the old 900s default, and
            // still not a finding, because its own TTL covered it. Under a
            // hardcoded threshold this would be reported.
            &ev_ttl("2026-08-01T11:00:00Z", "generous", "acquired", "b.rs", 7200),
            &ev_ttl("2026-08-01T11:30:00Z", "generous", "released", "b.rs", 7200),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(r.findings(), 1, "only the hold that outran its own TTL");
        assert_eq!(r.stale_holds[0].agent, "short");
        assert_eq!(r.stale_holds[0].ttl_secs, 600);
        assert!(!r.stale_holds[0].ttl_assumed);
    }

    /// pact-m7j.9.10: a bare `ttl_secs as i64` bit-reinterprets `u64::MAX` as
    /// `-1`, so a "hold forever" lease's TTL read back negative and every
    /// hold — however short — compared as `over` it. `Check::StaleHolds` has
    /// its own independent cast from `lease.rs`'s, so this pins it separately.
    #[test]
    fn a_u64_max_ttl_is_never_reported_as_stale() {
        let tmp = with_log(&[
            &ev_ttl(
                "2026-08-01T10:00:00Z",
                "forever",
                "acquired",
                "a.rs",
                u64::MAX,
            ),
            &ev_ttl(
                "2026-08-01T10:00:10Z",
                "forever",
                "released",
                "a.rs",
                u64::MAX,
            ),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "a u64::MAX ttl must never read back as already over its own TTL"
        );
    }

    /// Events written before pact recorded a TTL are judged against the default of
    /// THEIR era, not today's. Without this, raising the default would silently
    /// clear every historical finding — 22 of them in this repository — with
    /// nothing having changed about the holds.
    #[test]
    fn holds_with_no_recorded_ttl_use_the_legacy_default() {
        let tmp = with_log(&[
            // 20 minutes, no ttl_secs: over the 900s of its era, under a 2700s
            // present-day default.
            &ev("2026-08-01T10:00:00Z", "historic", "acquired", "a.rs"),
            &ev("2026-08-01T10:20:00Z", "historic", "released", "a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(
            r.findings(),
            1,
            "a pre-recording hold must still be judged against 900s"
        );
        let h = &r.stale_holds[0];
        assert_eq!(h.ttl_secs, LEGACY_DEFAULT_TTL_SECS);
        assert!(
            h.ttl_assumed,
            "and the report must say the TTL was inferred"
        );
        let text = render_check(&r);
        assert!(text.contains("predate pact recording a TTL"), "{text}");
    }

    /// A renew that grants a different TTL moves the window's threshold with it.
    #[test]
    fn a_renew_updates_the_ttl_the_hold_is_judged_against() {
        let tmp = with_log(&[
            &ev_ttl("2026-08-01T10:00:00Z", "a", "acquired", "a.rs", 600),
            // Renewed onto a much longer TTL, so the 30-minute hold is covered.
            &ev_ttl("2026-08-01T10:05:00Z", "a", "renewed", "a.rs", 7200),
            &ev_ttl("2026-08-01T10:30:00Z", "a", "released", "a.rs", 7200),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        // Renewals also disqualify it, which is the pre-existing rule; the point
        // here is that the recorded TTL followed the renew.
        assert_eq!(r.findings(), 0);
    }

    /// pact-m7j.1.3: `acquire_many`'s rollback logs a "restored" event when it
    /// undoes a refresh (lease.rs, pact-m7j.1.2). That "renewed" it undoes must
    /// stop counting toward the hold's renewal total — otherwise the phantom
    /// renewal exempts a hold from `stale-holds` even after it later genuinely
    /// lapses, which is exactly the case this check exists to catch.
    #[test]
    fn a_restored_hold_still_counts_as_never_renewed() {
        let tmp = with_log(&[
            // Pre-batch acquire under a 600s ttl.
            &ev_ttl("2026-08-01T10:00:00Z", "a", "acquired", "a.rs", 600),
            // A batch acquire refreshes it onto a much longer ttl...
            &ev_ttl("2026-08-01T10:01:00Z", "a", "renewed", "a.rs", 7200),
            // ...then fails on a later path and rolls the refresh back.
            &ev_ttl("2026-08-01T10:02:00Z", "a", "restored", "a.rs", 600),
            // No further activity: the restored 600s ttl lapses for real.
            &ev_ttl("2026-08-01T11:00:00Z", "a", "expired", "a.rs", 600),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(
            r.findings(),
            1,
            "the retracted renewal must not exempt a hold that really lapsed"
        );
        assert_eq!(r.stale_holds[0].renewals, 0, "the renewal was undone");
        assert_eq!(
            r.stale_holds[0].ttl_secs, 600,
            "judged against the restored ttl, not the batch's"
        );
        assert_eq!(r.stale_holds[0].closed_by.as_deref(), Some("expired"));
    }

    #[test]
    fn a_lapsed_lease_is_a_stale_hold_even_if_short() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "gone", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:05Z", "gone", "expired", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        assert_eq!(r.stale_holds[0].closed_by.as_deref(), Some("expired"));
    }

    /// A close-kind event with no matching open entry used to vanish from the
    /// reconstruction with no trace at all: no Hold, no counter, nothing.
    /// `orphaned_closes` is how that fact becomes visible instead of silent.
    #[test]
    fn a_close_with_no_open_is_counted_as_an_orphaned_close_not_silently_dropped() {
        let tmp = with_log(&[&ev("2026-08-01T10:00:00Z", "ghost", "released", "src/a.rs")]);

        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.events, 1, "the raw event is still counted");
        assert_eq!(
            s.orphaned_closes, 1,
            "but it closed no Hold, and that mismatch must be visible"
        );
        assert!(render_summary(&s).contains("no matching open"));

        // Every check that runs `reconstruct` reports the same count.
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.orphaned_closes, 1);
        assert!(render_check(&r).contains("no matching open"));
    }

    /// pact-m7j.2.6: `force-released` is filed under the agent who forced it,
    /// not the one displaced — `open.remove(&e.agent)` used to look for the
    /// FORCER's window, find nothing, count the close as orphaned, and leave
    /// the real holder's window running until they next touched the path.
    /// `displaced` is how the event names who to actually close.
    #[test]
    fn a_force_release_with_a_displaced_holder_closes_that_holders_window() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "victim", "acquired", "src/a.rs"),
            // The forcer's own name ("forcer") never opened anything on this
            // path, which is exactly why open.remove(&e.agent) found nothing
            // before this fix.
            r#"{"at":"2026-08-01T10:05:00Z","agent":"forcer","kind":"force-released","path":"src/a.rs","displaced":"victim"}"#,
        ]);

        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(
            s.orphaned_closes, 0,
            "the displaced holder's window must be found and closed, not orphaned"
        );
        assert_eq!(
            s.hold_secs.unwrap().completed,
            1,
            "one closed hold, victim's"
        );

        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.orphaned_closes, 0);
    }

    /// The historical shape (no `displaced` field at all, every event written
    /// before this fix) must keep behaving exactly as it did — `displaced`
    /// defaults to `None` and reconstruct falls back to `e.agent`, so a
    /// force-release with no way to name the real holder stays correctly
    /// orphaned rather than guessing.
    #[test]
    fn a_force_release_with_no_displaced_field_stays_orphaned_like_before() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "victim", "acquired", "src/a.rs"),
            &ev(
                "2026-08-01T10:05:00Z",
                "forcer",
                "force-released",
                "src/a.rs",
            ),
        ]);

        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(
            s.orphaned_closes, 1,
            "a pre-fix log with no displaced field must behave exactly as before"
        );
        assert_eq!(s.open_holds, 1, "victim's window is still open, as before");
    }

    #[test]
    fn an_unreleased_lease_shows_as_open_rather_than_guessed_at() {
        let tmp = with_log(&[&ev(
            "2026-08-01T10:00:00Z",
            "agent-a",
            "acquired",
            "src/a.rs",
        )]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.open_holds, 1);
        assert!(
            s.hold_secs.is_none(),
            "an open hold has no duration to average"
        );
    }

    fn annotation(covers: &[usize], note: &str) -> String {
        let lines: Vec<String> = covers.iter().map(|n| n.to_string()).collect();
        format!(
            r#"{{"at":"2026-08-06T12:00:00Z","agent":"maintainer","kind":"annotation","detail":"{note}","covers_lines":[{}],"actor":"maintainer"}}"#,
            lines.join(",")
        )
    }

    /// An annotation names lines that are not real history. They leave the
    /// statistics, and the fact that they left is reported — an exclusion nobody
    /// can see is indistinguishable from data that was never there.
    #[test]
    fn an_annotation_excludes_the_lines_it_covers_and_says_so() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "real", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:00Z", "real", "released", "src/a.rs"),
            &ev("2026-08-01T11:00:00Z", "ghost", "acquired", "ghost.rs"),
            &ev("2026-08-01T11:00:01Z", "ghost", "expired", "ghost.rs"),
            &annotation(&[3, 4], "synthetic: manual expiry experiment, agent ghost"),
        ]);

        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.events, 2, "the two synthetic events are gone");
        assert_eq!(s.excluded_by_annotation, 2);
        assert_eq!(s.agents, ["real"], "ghost is not an agent of this project");
        assert_eq!(s.annotations.len(), 1);
        assert_eq!(s.annotations[0].covers_lines, [3, 4]);
        assert_eq!(s.annotations[0].actor.as_deref(), Some("maintainer"));
        let text = render_summary(&s);
        assert!(text.contains("excluded by annotation"), "{text}");
        assert!(text.contains("maintainer"), "{text}");

        // And the raw log is still reachable, because the annotation is a claim
        // rather than a deletion.
        let raw = summary(tmp.path(), None, true).unwrap();
        assert_eq!(raw.events, 4);
        assert_eq!(raw.excluded_by_annotation, 0);
        assert!(raw.agents.contains(&"ghost".to_string()));
    }

    fn annotation_with_actor(covers: &[usize], note: &str, actor: &str) -> String {
        let lines: Vec<String> = covers.iter().map(|n| n.to_string()).collect();
        format!(
            r#"{{"at":"2026-08-06T12:00:00Z","agent":"maintainer","kind":"annotation","detail":"{note}","covers_lines":[{}],"actor":"{actor}"}}"#,
            lines.join(",")
        )
    }

    /// CURRENT (pre-fix) behavior, documented rather than changed: `actor` is
    /// forgeable free text pact never validated, and a malformed one still gets
    /// to exercise the exclusion exactly like a well-formed one — the mechanism
    /// chosen for this pass is to flag a bad actor, not to reject the
    /// correction it accompanies, precisely so this stays true.
    #[test]
    fn a_malformed_actor_still_excludes_its_covered_lines_like_a_well_formed_one() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "real", "acquired", "src/a.rs"),
            &ev("2026-08-01T11:00:00Z", "ghost", "acquired", "ghost.rs"),
            // "NOT-A-VALID-ACTOR!!" fails identity::validate: uppercase and
            // punctuation are both outside [a-z0-9][a-z0-9-]{1,31}.
            &annotation_with_actor(&[2], "synthetic", "NOT-A-VALID-ACTOR!!"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(
            s.excluded_by_annotation, 1,
            "a malformed actor does not stop the annotation from taking effect"
        );
        assert_eq!(s.events, 1);
    }

    /// The fix for this pass: an annotation whose `actor` fails
    /// `identity::validate`'s `[a-z0-9][a-z0-9-]{1,31}` check is now flagged as
    /// such — distinctly from a well-formed actor and from no actor at all —
    /// both in the struct (`actor_valid`) and in the rendered report.
    #[test]
    fn an_annotation_with_an_invalid_actor_is_flagged_distinctly() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "real", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:00Z", "real", "released", "src/a.rs"),
            // Excluded by the well-formed annotation below, so
            // `excluded_by_annotation > 0` and the annotation section of the
            // rendered summary actually runs.
            &ev("2026-08-01T11:00:00Z", "ghost", "acquired", "ghost.rs"),
            &annotation_with_actor(&[3], "well-formed", "maintainer"),
            &annotation_with_actor(&[], "malformed", "NOT-A-VALID-ACTOR!!"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.events, 2, "the ghost line is excluded, real ones are not");
        assert_eq!(s.annotations.len(), 2);
        let good = s.annotations.iter().find(|a| a.line == 4).unwrap();
        let bad = s.annotations.iter().find(|a| a.line == 5).unwrap();
        assert!(good.actor_valid, "a well-formed actor is not flagged");
        assert!(
            !bad.actor_valid,
            "an actor failing identity::validate must be flagged invalid"
        );

        let text = render_summary(&s);
        assert!(
            text.contains("INVALID ACTOR"),
            "the report must call out the bad one distinctly: {text}"
        );
        // And the well-formed line must NOT carry the same flag.
        let good_line = text
            .lines()
            .find(|l| l.contains("line 4"))
            .expect("line 4 rendered");
        assert!(!good_line.contains("INVALID ACTOR"), "{good_line}");
    }

    /// An absent actor is a different, already-surfaced condition ("unknown")
    /// and must not be flagged as malformed — that would conflate "nobody
    /// signed this" with "someone signed this with garbage".
    #[test]
    fn an_absent_actor_is_not_flagged_invalid() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "real", "acquired", "src/a.rs"),
            r#"{"at":"2026-08-06T12:00:00Z","agent":"maintainer","kind":"annotation","detail":"no actor field","covers_lines":[1]}"#,
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.annotations.len(), 1);
        assert!(s.annotations[0].actor.is_none());
        assert!(
            s.annotations[0].actor_valid,
            "absent is not the same as invalid"
        );
        assert!(!render_summary(&s).contains("INVALID ACTOR"));
    }

    /// The assertion the whole mechanism exists for: an annotated double-win must
    /// NOT fire the check, because pact-ehi's trigger condition counts real
    /// history only — while an unannotated one still must.
    #[test]
    fn an_annotated_double_win_does_not_fire_but_a_real_one_does() {
        let overlapping = [
            ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            ev("2026-08-01T10:00:30Z", "agent-b", "acquired", "src/a.rs"),
        ];

        // Unannotated: a finding, and the guard-file trigger has fired.
        let live = with_log(&[&overlapping[0], &overlapping[1]]);
        assert_eq!(
            run_check(live.path(), Check::DoubleWin, None, false)
                .unwrap()
                .findings(),
            1
        );

        // The same two events, annotated as synthetic: no finding.
        let annotated = with_log(&[
            &overlapping[0],
            &overlapping[1],
            &annotation(&[1, 2], "synthetic: hand-run steal experiment"),
        ]);
        let r = run_check(annotated.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0, "an annotated overlap is not evidence");
        assert_eq!(r.excluded_by_annotation, 2);
        assert!(
            render_check(&r).contains("excluded by annotation"),
            "the check must admit it skipped events: {}",
            render_check(&r)
        );

        // ...and --include-annotated shows it again, so the annotation can be
        // disputed rather than merely trusted.
        assert_eq!(
            run_check(annotated.path(), Check::DoubleWin, None, true)
                .unwrap()
                .findings(),
            1
        );
    }

    /// An annotation covering one half of an overlap still removes the overlap:
    /// exclusion is per event, and a window needs both ends.
    #[test]
    fn annotating_one_side_of_an_overlap_is_enough() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "real", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:30Z", "ghost", "acquired", "src/a.rs"),
            &annotation(&[2], "synthetic: only the ghost half"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0);
        assert_eq!(r.excluded_by_annotation, 1);
    }

    /// The annotation row is never counted as history itself, in either mode —
    /// otherwise every correction would inflate the event total.
    #[test]
    fn the_annotation_row_is_not_itself_an_event() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "real", "acquired", "src/a.rs"),
            &annotation(&[99], "covers a line that does not exist"),
        ]);
        for include in [false, true] {
            let s = summary(tmp.path(), None, include).unwrap();
            assert_eq!(s.events, 1, "include_annotated={include}");
            assert_eq!(s.unparseable_lines, 0, "an annotation parses fine");
        }
    }

    #[test]
    fn since_accepts_both_spellings_and_filters() {
        assert!(parse_since("2026-08-01T00:00:00Z").is_ok());
        for d in ["30s", "15m", "6h", "7d", "2w"] {
            assert!(parse_since(d).is_ok(), "{d}");
        }
        for bad in ["", "7", "7y", "yesterday", "d7"] {
            assert!(parse_since(bad).is_err(), "{bad} should not parse");
        }

        let tmp = with_log(&[
            &ev("2020-01-01T00:00:00Z", "old", "acquired", "src/a.rs"),
            &ev("2020-01-01T00:01:00Z", "old", "released", "src/a.rs"),
        ]);
        let cut = parse_since("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(
            summary(tmp.path(), Some(cut), false).unwrap().events,
            0,
            "--since must exclude older events"
        );
        assert_eq!(summary(tmp.path(), None, false).unwrap().events, 2);
    }

    /// Junk that is not JSON at all, in the middle rather than at the end. Still
    /// counted, still skipped, and the surrounding events still analysed.
    #[test]
    fn junk_lines_anywhere_do_not_derail_the_scan() {
        let tmp = with_log(&[
            "this is not json",
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            "",
            "{}",
            &ev("2026-08-01T10:01:00Z", "agent-a", "released", "src/a.rs"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.events, 2);
        // `{}` fails to deserialize (no required fields) and the blank line is
        // skipped without counting.
        assert_eq!(s.unparseable_lines, 2);
    }

    // ----------------------------------------------------------- chain-integrity

    /// A real, chain-hashed `Event`, built through the same struct pact itself
    /// writes rather than through `ev()`'s bare JSON — `chain_hash` is computed
    /// by `events::append`, so the fixture must go through it to get one at all.
    fn chain_event(agent: &str, kind: &str, path: &str) -> Event {
        Event {
            at: Utc::now().to_rfc3339(),
            agent: agent.to_string(),
            kind: kind.to_string(),
            path: Some(path.to_string()),
            detail: None,
            ttl_secs: None,
            covers_lines: None,
            actor: None,
            displaced: None,
            chain_hash: None,
        }
    }

    /// pact-m7j.2.5's acceptance criteria: a hand-edited `chain_hash` — the
    /// shape a forged or tampered line actually takes on disk, since nobody but
    /// `append_bounded` can compute one that verifies — must be flagged, and
    /// flagged distinctly from the genuine lines around it.
    #[test]
    fn a_hand_edited_chain_hash_is_flagged_distinctly_from_genuine_history() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        crate::events::append(tmp.path(), &chain_event("agent-a", "acquired", "src/a.rs"));
        crate::events::append(tmp.path(), &chain_event("agent-a", "released", "src/a.rs"));

        let log_path = tmp.path().join(".pact").join("events.jsonl");
        let contents = std::fs::read_to_string(&log_path).unwrap();
        let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
        assert_eq!(
            lines.len(),
            2,
            "fixture must have written exactly two lines"
        );
        let mut tampered: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        tampered["chain_hash"] = serde_json::Value::String("0000000000000000".to_string());
        lines[1] = tampered.to_string();
        std::fs::write(&log_path, lines.join("\n") + "\n").unwrap();

        let r = run_check(tmp.path(), Check::ChainIntegrity, None, false).unwrap();
        assert_eq!(r.findings(), 1, "exactly the tampered line, nothing else");
        assert_eq!(r.chain_breaks[0].line, 2);
        assert_eq!(r.chain_tracked, 2, "both lines still carry SOME chain_hash");
        assert_eq!(r.chain_untracked, 0);

        let text = render_check(&r);
        assert!(text.contains("CHAIN BREAK"), "{text}");
        assert!(text.contains("line 2"), "{text}");
    }

    /// The other half of the same acceptance criteria: a log with NO
    /// `chain_hash` anywhere — every log written before pact-m7j.2.5, including
    /// this repository's own committed history — must report cleanly. A missing
    /// field is not evidence of tampering; treating it as such would flag every
    /// pre-existing repository the moment this shipped.
    #[test]
    fn a_pre_existing_history_log_with_no_chain_hash_anywhere_reports_cleanly() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:00Z", "agent-a", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::ChainIntegrity, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "no chain_hash anywhere must not read as tampering"
        );
        assert_eq!(r.chain_tracked, 0);
        assert_eq!(r.chain_untracked, 2);

        let text = render_check(&r);
        assert!(!text.contains("CHAIN BREAK"), "{text}");
        assert!(text.contains("predate chain tracking"), "{text}");
    }

    /// A forged line appended with no `chain_hash` of its own — the bead's other
    /// named scenario — is not a mismatch (there is nothing on it to mismatch),
    /// but it must show up as untracked rather than silently extending the
    /// tracked run, so a reader can see tracking stopped where it should not
    /// have.
    #[test]
    fn a_forged_line_with_no_chain_hash_after_a_real_chain_counts_as_untracked_not_a_break() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        crate::events::append(tmp.path(), &chain_event("agent-a", "acquired", "shared.rs"));

        // Hand-appended: a forged "released" for a path a peer still holds,
        // with no chain_hash field at all — exactly what appending via a text
        // editor rather than `pact` produces.
        let log_path = tmp.path().join(".pact").join("events.jsonl");
        let mut forged = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        writeln!(
            forged,
            "{}",
            ev("2026-08-06T00:00:00Z", "attacker", "released", "shared.rs")
        )
        .unwrap();
        drop(forged);

        let r = run_check(tmp.path(), Check::ChainIntegrity, None, false).unwrap();
        assert_eq!(r.findings(), 0, "a missing hash is not a mismatch");
        assert_eq!(r.chain_tracked, 1);
        assert_eq!(r.chain_untracked, 1);
    }
}
