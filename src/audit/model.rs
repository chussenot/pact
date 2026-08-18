//! The log read back as history: what an event means, and what the holds it
//! describes actually were.
//!
//! Everything here answers a question about the event stream itself rather than
//! about any one check — which kinds open and close a hold, how long a hold ran,
//! and which of them overlapped. [`reconstruct`] is the single pass every check
//! and the summary are built on, and the double-win detection lives inside it
//! rather than in `checks/double_win.rs` for a reason worth stating: an overlap
//! is only visible while walking the open windows, so separating the two would
//! mean walking the log twice and keeping two copies of the same
//! re-entrant/takeover argument in step.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::events::Event;

/// The default TTL before pact recorded it per-event, used for holds whose
/// opening event carries no `ttl_secs`.
///
/// NOT `DEFAULT_TTL_SECS`. Judging a historical hold against today's compiled
/// default is how raising that default silently rewrites the past: every hold in
/// this repository's log was taken under a 900s TTL and none exceeds 36m, so under
/// a 45m default all 22 findings would vanish without anything having changed
/// about them. A hold is compared against the TTL that was actually in force.
pub(in crate::audit) const LEGACY_DEFAULT_TTL_SECS: u64 = 900;

/// The fallback only means anything while the compiled default has moved past it:
/// if the two are equal again, `holds_with_no_recorded_ttl_use_the_legacy_default`
/// silently stops testing anything. A `const` assertion rather than one inside the
/// test, because comparing two constants at run time is a clippy lint and this is
/// knowable at compile time anyway.
const _: () = assert!(crate::lease::DEFAULT_TTL_SECS > LEGACY_DEFAULT_TTL_SECS);

/// Is this "agent" actually the fault injector rather than a fleet member?
///
/// `scripts/chaos.sh` acquires as `chaos-ghost` to plant a stale lease, so its
/// refusals are a rail firing correctly, not a peer misbehaving. Reporting them
/// would credit the fleet with waste it did not cause.
pub(in crate::audit) fn is_injector(agent: &str) -> bool {
    agent == "chaos-ghost"
}

/// A hold that has been opened and not yet closed, while `reconstruct` walks the log.
///
/// A named struct rather than the tuple this was, because pact-b73.6 needed to carry a
/// sixth field (`head`) and a six-tuple destructured at four sites is a positional
/// mistake waiting to happen.
#[derive(Debug)]
struct OpenWindow {
    line: usize,
    at: String,
    renewals: usize,
    ttl: u64,
    ttl_assumed: bool,
    /// Short HEAD recorded on the opening event, when it recorded one.
    head: Option<String>,
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
    /// Short HEAD at the moment the hold opened, if the event recorded one.
    ///
    /// With both heads present, a hold brackets an EXACT commit range — `git log
    /// open..close` — so "what did this agent land under this lease" stops being an
    /// inference from timestamps. Absent on every log written before pact stamped it,
    /// which is why `commit-correlation` keeps its timestamp path (pact-b73.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_head: Option<String>,
    /// Short HEAD at the moment the hold closed. `None` on an open hold, and on an
    /// `expired` close — the holder was gone, so HEAD then belongs to whoever swept
    /// the lock rather than to the work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_head: Option<String>,
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

pub(in crate::audit) fn parse_at(at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Does this kind open a hold window?
///
/// `stolen` counts, and that is not a detail: a takeover is a hold. Treating it
/// as anything else would make every reclaim invisible to `stale-holds` and would
/// hide exactly the double-wins a `--steal` could cause.
pub(in crate::audit) fn opens(kind: &str) -> bool {
    matches!(kind, "acquired" | "stolen")
}

/// Does this kind close one?
///
/// `expired` closes the **previous** holder's window: `lease.rs` logs it under
/// `existing.agent`, not under the agent taking over. That is what makes a
/// routine reclaim — `expired` then `stolen` — read correctly instead of looking
/// like two agents holding at once.
///
/// A forced `--steal` is NOT in this list, and deliberately so: it closes the
/// displaced holder from inside the `opens` arm, after recording the overlap the
/// steal genuinely represents (pact-mqw.1). `displaced` is not here either — it
/// is a feed row, and the `stolen` before it already did the closing.
pub(in crate::audit) fn closes(kind: &str) -> bool {
    matches!(kind, "released" | "force-released" | "expired")
}

/// Close `agent`'s open window on `path`, if it has one, recording the finished
/// hold. Returns whether anything was closed.
///
/// Split out for the two takeover sites (pact-mqw.1), which close a window
/// belonging to somebody other than the event's own author and must stay silent
/// when there is nothing open — unlike the ordinary close arm, whose whole job
/// includes counting a close that found no window as orphaned.
#[allow(clippy::too_many_arguments)]
fn close_window(
    open: &mut BTreeMap<String, OpenWindow>,
    holds: &mut Vec<Hold>,
    path: &str,
    agent: &str,
    line: usize,
    at: &str,
    closed_by: &str,
    // HEAD on the CLOSING event, so a takeover site can pass the closer's rather
    // than the displaced holder's.
    closing_head: Option<&str>,
) -> bool {
    let Some(w) = open.remove(agent) else {
        return false;
    };
    let held = parse_at(&w.at)
        .zip(parse_at(at))
        .map(|(a, b)| (b - a).num_seconds());
    holds.push(Hold {
        path: path.to_string(),
        agent: agent.to_string(),
        opened_line: w.line,
        opened_at: w.at,
        closed_line: Some(line),
        closed_at: Some(at.to_string()),
        closed_by: Some(closed_by.to_string()),
        renewals: w.renewals,
        held_secs: held,
        ttl_secs: w.ttl,
        ttl_assumed: w.ttl_assumed,
        open_head: w.head,
        close_head: closing_head.map(str::to_string),
    });
    true
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
pub(in crate::audit) fn reconstruct(
    events: &[(usize, Event)],
) -> (Vec<Hold>, Vec<DoubleWin>, usize) {
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
        let mut open: BTreeMap<String, OpenWindow> = BTreeMap::new();

        for (line, e) in rows {
            if opens(&e.kind) {
                let others: Vec<HoldingAgent> = open
                    .iter()
                    .filter(|(a, _)| *a != &e.agent)
                    .map(|(a, w)| HoldingAgent {
                        agent: a.clone(),
                        since: w.at.clone(),
                        since_line: w.line,
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
                open.entry(e.agent.clone()).or_insert(OpenWindow {
                    line,
                    at: e.at.clone(),
                    renewals: 0,
                    ttl: e.ttl_secs.unwrap_or(LEGACY_DEFAULT_TTL_SECS),
                    ttl_assumed: e.ttl_secs.is_none(),
                    head: e.head.clone(),
                });
                // pact-mqw.1: a takeover ENDS the displaced holder's claim, and
                // until now nothing said so. A routine reclaim gets an `expired`
                // row to close it, but a `--steal` over a live lease had none —
                // and a SIGKILLed holder emits no `released` either, so its
                // window stayed open for the remainder of the log and every later
                // acquire of the path was reported as an overlap against a holder
                // that had been gone for minutes. Nine such reports on the
                // crucible log, eight naming one killed agent, none of them a
                // concurrent hold.
                //
                // Closing here rather than on the `displaced` row that lease.rs
                // now writes, because this works on EVERY log: the false
                // positives are in logs already on disk, written by versions that
                // never emitted `displaced`. The overlap has already been
                // recorded just above, so the steal itself is still reported —
                // that part is deliberate.
                if e.kind == "stolen" {
                    let victims: Vec<String> =
                        open.keys().filter(|a| *a != &e.agent).cloned().collect();
                    for victim in victims {
                        close_window(
                            &mut open,
                            &mut holds,
                            path,
                            &victim,
                            line,
                            &e.at,
                            "stolen",
                            e.head.as_deref(),
                        );
                    }
                }
            } else if e.kind == "displaced" {
                // Primarily a feed row: it stops `pact log` and `pact ui` naming
                // an overridden agent as the current holder. The `stolen` row
                // immediately before it has normally closed this window already,
                // so this is usually a no-op — but it closes if anything IS still
                // open, which covers a bounded or truncated log whose `stolen`
                // line was trimmed away. Silent when there is nothing to close:
                // unlike the close arm below it must not count that as an orphan,
                // because the expected case is that the steal got there first.
                close_window(
                    &mut open,
                    &mut holds,
                    path,
                    &e.agent,
                    line,
                    &e.at,
                    "displaced",
                    e.head.as_deref(),
                );
            } else if e.kind == "renewed" {
                if let Some(slot) = open.get_mut(&e.agent) {
                    slot.renewals += 1;
                    // A renew can change the TTL, so the window adopts the newest
                    // one it was actually granted.
                    if let Some(ttl) = e.ttl_secs {
                        slot.ttl = ttl;
                        slot.ttl_assumed = false;
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
                    slot.renewals = slot.renewals.saturating_sub(1);
                    if let Some(ttl) = e.ttl_secs {
                        slot.ttl = ttl;
                        slot.ttl_assumed = false;
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
                if let Some(w) = open.remove(holder) {
                    let held = parse_at(&w.at)
                        .zip(parse_at(&e.at))
                        .map(|(a, b)| (b - a).num_seconds());
                    holds.push(Hold {
                        path: path.to_string(),
                        agent: holder.to_string(),
                        opened_line: w.line,
                        opened_at: w.at,
                        closed_line: Some(line),
                        closed_at: Some(e.at.clone()),
                        closed_by: Some(e.kind.clone()),
                        renewals: w.renewals,
                        held_secs: held,
                        ttl_secs: w.ttl,
                        ttl_assumed: w.ttl_assumed,
                        open_head: w.head,
                        close_head: e.head.clone(),
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
        for (agent, w) in open {
            holds.push(Hold {
                path: path.to_string(),
                agent,
                opened_line: w.line,
                opened_at: w.at,
                closed_line: None,
                closed_at: None,
                closed_by: None,
                renewals: w.renewals,
                held_secs: None,
                ttl_secs: w.ttl,
                ttl_assumed: w.ttl_assumed,
                open_head: w.head,
                close_head: None,
            });
        }
    }

    (holds, doubles, orphaned_closes)
}
