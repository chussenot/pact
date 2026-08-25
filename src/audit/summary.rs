//! `pact audit` with no `--check`: the distribution, not a verdict.
//!
//! The summary answers "what did this run DO" — how many events, held by whom,
//! for how long, contended how hard — and deliberately never says whether that
//! was good. Every named check is a judgement against a stated rule; this is the
//! description a reader forms one from.
//!
//! [`Summary`] and [`render_summary`] stay in one file because the struct is a
//! report format: every field exists because the rendering says something about
//! it, and the comments on the fields are where the reasons live. Splitting them
//! would leave a field and its justification in different files.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::context::{load, Annotation};
use super::model::{opens, reconstruct, Hold};
use super::secs;
use crate::events::Event;

/// How many contended paths and agents a summary lists before it stops being a
/// summary. The full data is in the log; this is the part a human reads.
const TOP_N: usize = 10;

/// How much coordination work a run spent, against what it bought.
///
/// From the crucible run, the first with real contention data: 124 refusals for 58
/// acquires and 5 takeovers — about 2 refusals per successful claim — with 9
/// (agent, path) pairs refused and never acquired at all, 16 refusals spent on
/// paths their asker never got.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Contention {
    pub refusals: usize,
    /// Acquires plus takeovers: every way a claim actually landed.
    pub claims: usize,
    /// Distinct (agent, path) pairs that were refused at least once.
    pub contended_pairs: usize,
    /// Pairs refused and never subsequently claimed by that agent — work the fleet
    /// spent asking for something it never got.
    pub abandoned_pairs: usize,
    /// Refusals belonging to those abandoned pairs.
    pub abandoned_refusals: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contended {
    pub path: String,
    pub holds: usize,
    pub distinct_agents: usize,
    /// This lease stands for something other than a file — see [`is_mutex`].
    ///
    /// Reported rather than filtered. A mutex hold is still coordination and still
    /// evidence; what it must not do is sit in a "most contended paths" table above
    /// real source files, which is what `.beads` did in the quern run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mutex: bool,
}

/// Re-exported from [`crate::lease`], where the definition now lives.
///
/// "Is this lease a mutex rather than a claim on a file?" is a question about a
/// lease, and three callers outside audit ask it — `lease` itself when deciding
/// whether an absent path is worth warning about, `watch` when deciding whether a
/// release can carry a diff, and `merge` when naming its key. Keeping the
/// definition here made `audit` a dependency of all of them, which the benches
/// (which compile `lease` and `watch` without it) refused to link.
///
/// Imported rather than re-exported: every caller now names `lease::is_mutex`
/// directly, and a `pub use` here would only offer a second name for one thing.
use crate::lease::is_mutex;

#[derive(Debug, Clone, Serialize)]
pub struct AgentActivity {
    pub agent: String,
    pub events: usize,
    pub holds: usize,
    pub steals: usize,
    /// Takeovers of a lease that had already lapsed — an `expired` row for the
    /// path precedes them. Counted apart from `steals`, which is reserved for a
    /// forced `--steal` over a claim that was still live (pact-mqw.2).
    #[serde(default)]
    pub reclaims: usize,
    pub held_secs_total: i64,
    /// The attribution chain this agent's rows carried, last-wins (pact-c3y).
    ///
    /// Attribution, never judgment. Nothing here can fail a check and nothing
    /// here is compared against an expectation: an agent that declared no model
    /// is not doing anything wrong, and a summary that implied otherwise would
    /// train fleets to declare something rather than the right thing.
    ///
    /// Last-wins rather than first, matching `events::context_at_end`: an agent
    /// re-spawned onto a different model mid-run genuinely changed, and the
    /// earlier rows stay in the log saying so.
    ///
    /// Each is `None` when no row of this agent's carried it — which, until
    /// fleets start declaring, is most of them, and is why every consumer here
    /// degrades to omitting the column rather than to a placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// How much of the plan's inheritance actually got written.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffCoverage {
    /// Beads the plan gives at least one dependent.
    pub with_dependents: usize,
    /// Of those, how many have a handoff on record.
    pub handed_off: usize,
    /// The ones that have not — named, because a count nobody can act on is a
    /// number rather than a finding. Capped at [`TOP_N`] like every other list
    /// here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub silent: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HoldStats {
    pub completed: usize,
    pub median_secs: i64,
    pub p90_secs: i64,
    pub max_secs: i64,
    /// Holds that ended by lapsing rather than by `lease release`.
    ///
    /// NOT a fault count. Measured on the quern run, 2 of 3 expiries were deliberate
    /// short-lived mutexes — `ttl=20` to file nine beads, `ttl=120` for a `bd close` —
    /// let go by lapsing on purpose. A 20-second lease that expired did exactly what
    /// its holder asked. Reported because the alternative is a reader inferring it
    /// from `by_kind`, and separated from short TTLs below so the number can be read
    /// without accusing anyone.
    #[serde(default)]
    pub ended_by_expiry: usize,
    /// Of [`Self::ended_by_expiry`], the ones whose own recorded TTL was under
    /// [`SHORT_TTL_SECS`] — a lock taken to serialize a quick write, not a lease
    /// somebody abandoned. Judged against the TTL the event RECORDED, never against
    /// the current default, for the same reason `stale-holds` does: the default has
    /// changed, and re-judging old history by today's number rewrites verdicts.
    #[serde(default)]
    pub expiry_short_ttl: usize,
}

/// Which beads with dependents left findings for them.
///
/// From `plan.json` and `messages.jsonl` only — no bd, no subprocess, and no
/// guess at edges. `None` where no plan has been linted, because coverage against
/// a graph nobody declared is not a low number, it is an unanswerable question.
fn handoff_coverage(repo_root: &std::path::Path) -> Option<HandoffCoverage> {
    let snapshot = crate::plan::snapshot(repo_root)?;
    // The structured field, not the subject line: `handoff_from` exists precisely
    // so this arithmetic does not depend on a sentence anybody could reword.
    let sent: BTreeSet<String> = crate::msg::all_records(repo_root)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| r.handoff_from)
        .collect();
    let with_dependents: Vec<String> = snapshot
        .edges
        .keys()
        .filter(|id| !snapshot.dependents(id).is_empty())
        .cloned()
        .collect();
    let mut silent: Vec<String> = with_dependents
        .iter()
        .filter(|id| !sent.contains(*id))
        .cloned()
        .collect();
    silent.sort();
    silent.truncate(TOP_N);
    Some(HandoffCoverage {
        handed_off: with_dependents
            .iter()
            .filter(|id| sent.contains(*id))
            .count(),
        with_dependents: with_dependents.len(),
        silent,
    })
}

/// Below this, a lapsed lease reads as a deliberate short-lived lock rather than an
/// abandoned hold.
///
/// Five minutes, against a 45-minute default. The observed idiom sat far below it
/// (20s, 120s, 180s) and real work sat far above (the same run's median hold was
/// 4m34s with a 2700s TTL), so the boundary is not close to anything it has to
/// separate.
pub const SHORT_TTL_SECS: i64 = 300;

#[derive(Debug, Serialize)]
pub struct Summary {
    /// The constraints this run operated under, from `pact context set`.
    ///
    /// Printed in the header rather than buried, because it is what decides how
    /// every number below should be read: "8 holds on one file" means one thing
    /// under a free-running scheduler and another under `scheduler=pre-serialized`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub context: std::collections::BTreeMap<String, String>,
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
    /// Takeovers of a lease that had already lapsed — an `expired` row for the
    /// path precedes them. Counted apart from `steals`, which is reserved for a
    /// forced `--steal` over a claim that was still live (pact-mqw.2).
    #[serde(default)]
    pub reclaims: usize,
    /// Contention, related to what it achieved (pact-1gv.4).
    ///
    /// A bare refusal count is uninterpretable: 124 refusals reads equally well as
    /// "healthy contention that resolved" and "a fleet thrashing", and until this
    /// there was no way to tell which. The ratio is where the meaning is, and it is
    /// only useful as a trend, which is why these live on `Summary` — `--compare`'s
    /// field table is how pact tracks trends across runs.
    #[serde(default)]
    pub contention: Contention,
    pub open_holds: usize,
    pub hold_secs: Option<HoldStats>,
    pub top_contended: Vec<Contended>,
    pub per_agent: Vec<AgentActivity>,
    /// How many EVENTS each declared model wrote, and how many declared none
    /// (pact-c3y).
    ///
    /// By events rather than by agent, deliberately: an agent that acquired once
    /// and one that ran the whole build are not equal evidence about what a run
    /// was made of, and counting heads would say they were.
    ///
    /// `model_undeclared` is the load-bearing half. Without it, a run where one
    /// agent declared and nineteen did not reads as a single-model fleet — the
    /// exact wrong conclusion, and the one a bare histogram invites.
    ///
    /// Attribution, not judgment: nothing here fails a check. A fleet is allowed
    /// to declare nothing, and most do.
    /// Beads the plan says have dependents, and how many of them left findings
    /// for those dependents (pact-e7d).
    ///
    /// **A COUNT, never a verdict.** A bead with nothing worth saying should send
    /// nothing, so a gap here is a smell to look at rather than a rule anybody
    /// broke — which is why it lives in the summary, which never judges, rather
    /// than in a `--check`, which always does.
    ///
    /// The brief this came from asked for "closed beads with dependents that sent
    /// no handoff", computed from `plan.json` and `messages.jsonl` alone. Those two
    /// sources cannot say which beads are CLOSED — that lives in bd, which audit
    /// reads only through the committed interactions export and which the
    /// constraint deliberately excludes. So this measures what they can answer:
    /// every bead the plan gives dependents, and whether a handoff from it exists.
    /// An open bead with no handoff is counted the same as a closed one, and it
    /// should be — it has not sent yet, and the number is a coverage figure rather
    /// than an accusation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_coverage: Option<HandoffCoverage>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub models_by_events: BTreeMap<String, usize>,
    #[serde(default)]
    pub model_undeclared: usize,
    /// How many events came from each invocation point — a linked worktree's
    /// name, `main`, `outside`, or `unknown` for events written before pact
    /// recorded it at all (pact-ler.1/.2).
    ///
    /// The question this answers is "did this run use the topology I asked
    /// for", which nothing could answer before: a 20-agent run with one git
    /// worktree per agent produced events indistinguishable from a plain
    /// single-checkout run.
    pub by_invoked_from: BTreeMap<String, usize>,
    /// Which revision(s) of the managed protocol block the events in this
    /// window were written under (pact-okz.1), with a count each.
    ///
    /// More than one entry means the protocol changed mid-window, which is
    /// exactly the case a before/after comparison must not average over — and
    /// exactly the case that produced a wrong conclusion when it had to be
    /// established by git archaeology instead. `unknown` counts events from
    /// before pact recorded it.
    pub by_protocol: BTreeMap<String, usize>,
    /// Events written before pact recorded the protocol at all (pact-b73.1).
    ///
    /// Counted here rather than under an `"unknown"` key in `by_protocol`,
    /// and the distinction is not cosmetic: folding it in made a run that
    /// merely spanned a **binary upgrade** report that the protocol had
    /// changed. grimcast did exactly that — 159 events from pact 0.7.2, which
    /// predates the stamp, and 104 from 0.7.4, all under one unchanged block —
    /// and was told the block changed underneath it. Same discipline as
    /// `chain_untracked` and the topology check: "not recorded" is never a
    /// value.
    pub protocol_unstamped: usize,
    /// Which pact versions wrote the events in this window.
    ///
    /// More than one means the binary was upgraded mid-run, which changes what
    /// the log is even able to record — and is the thing that explains a
    /// sudden appearance of any newer field. Nothing surfaced it before, so
    /// the only symptom was a field that "started existing" halfway through.
    pub by_pact_version: BTreeMap<String, usize>,
    /// Subscriptions in force right now (pact-8qu). Live state, not history —
    /// read from `.pact/watches.jsonl` rather than reconstructed from the
    /// event log, because a `watched` event says a subscription was created,
    /// not that it still stands.
    pub watches_active: usize,
    /// `notified` events: diffs actually delivered to a subscriber.
    pub diffs_delivered: usize,
    /// `watch-delivery-failed` events. Reported next to `diffs_delivered`
    /// rather than only when nonzero, because delivery is best-effort by
    /// design and a reader needs to know whether "0 delivered" means nothing
    /// changed or means nothing got through.
    pub deliveries_failed: usize,
    /// Set only when this repository **currently has linked worktrees** and
    /// not one event was invoked from any of them.
    ///
    /// Deliberately not inferred from merge-commit shape, which was the
    /// obvious heuristic and is a bad one: a repo that merges ordinary feature
    /// branches has exactly the same commit shape, so the hint would fire on
    /// most repositories and mean nothing. docs/audit.md already records what
    /// that costs — "a metric that returns the same answer regardless of the
    /// behaviour it claims to measure is worse than no metric, because it
    /// looks like evidence". `has_worktrees` is a fact about this repository
    /// right now, so this cannot false-positive; it simply says nothing about
    /// worktrees that have since been removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topology_note: Option<String>,
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
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
    // pact-mqw.2: "stolen" covers both halves of a takeover, and counting it flat
    // reports routine reclaims as aggression. The two are distinguishable and
    // always have been — lease.rs writes an `expired` row under the dead holder
    // before a reclaim's `stolen`, and a forced `--steal` never has one — but
    // this summary was not using that. On the crucible log the flat count read
    // "5 steals" for 3 forced overrides, and credited an agent with "2 steal(s)"
    // that had never passed --steal in its life.
    let mut prev_kind_for_path: BTreeMap<&str, &str> = BTreeMap::new();
    let mut forced_total = 0usize;
    let mut reclaim_total = 0usize;
    for (_, e) in &events {
        *by_kind.entry(e.kind.clone()).or_insert(0) += 1;
        agents.insert(e.agent.clone());
        let path = e.path.as_deref().unwrap_or_default();
        let takeover_of_a_lapsed_lease =
            e.kind == "stolen" && prev_kind_for_path.get(path) == Some(&"expired");
        let a = per.entry(e.agent.clone()).or_insert_with(|| AgentActivity {
            agent: e.agent.clone(),
            events: 0,
            holds: 0,
            steals: 0,
            reclaims: 0,
            held_secs_total: 0,
            harness: None,
            model: None,
            branch: None,
        });
        a.events += 1;
        // Last-wins, and `or` on the ROW rather than on the accumulator: a row
        // that carries nothing must not erase what an earlier row said, because
        // most kinds in most logs carry nothing and the first `released` after an
        // `acquired` would otherwise blank the agent out.
        a.harness = e.harness.clone().or_else(|| a.harness.take());
        a.model = e.model.clone().or_else(|| a.model.take());
        a.branch = e.branch.clone().or_else(|| a.branch.take());
        if e.kind == "stolen" {
            if takeover_of_a_lapsed_lease {
                a.reclaims += 1;
                reclaim_total += 1;
            } else {
                a.steals += 1;
                forced_total += 1;
            }
        }
        if !path.is_empty() {
            prev_kind_for_path.insert(path, e.kind.as_str());
        }
    }
    for h in &holds {
        if let Some(a) = per.get_mut(&h.agent) {
            a.holds += 1;
            a.held_secs_total += h.held_secs.unwrap_or(0);
        }
    }

    // Computed over EVERY event, not over `per_agent`, which is truncated to
    // TOP_N: the models line is about the whole run, and taking it from the
    // top-ten table would silently drop the eleventh agent's model.
    let mut models_by_events: BTreeMap<String, usize> = BTreeMap::new();
    let mut model_undeclared = 0usize;
    for (_, e) in &events {
        match e.model.as_deref() {
            Some(m) => *models_by_events.entry(m.to_string()).or_default() += 1,
            None => model_undeclared += 1,
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
            mutex: is_mutex(p),
        })
        .collect();
    // Mutexes sort BELOW every file, whatever their hold count. They are reported —
    // see `Contended::mutex` — but a lock that stands for "the bd store" is not
    // competing for a source file's attention, and in the quern run `.beads` sat
    // second in this table, above every real file it outranked on hold count alone.
    top_contended.sort_by(|a, b| {
        a.mutex
            .cmp(&b.mutex)
            .then(b.distinct_agents.cmp(&a.distinct_agents))
            .then(b.holds.cmp(&a.holds))
            .then(a.path.cmp(&b.path))
    });
    top_contended.truncate(TOP_N);

    let mut durations: Vec<i64> = holds.iter().filter_map(|h| h.held_secs).collect();
    durations.sort_unstable();
    let expired: Vec<&Hold> = holds
        .iter()
        .filter(|h| h.closed_by.as_deref() == Some("expired"))
        .collect();
    let ended_by_expiry = expired.len();
    let expiry_short_ttl = expired
        .iter()
        .filter(|h| (h.ttl_secs as i64) < SHORT_TTL_SECS)
        .count();
    let hold_secs = (!durations.is_empty()).then(|| HoldStats {
        completed: durations.len(),
        ended_by_expiry,
        expiry_short_ttl,
        median_secs: percentile(&durations, 0.5),
        p90_secs: percentile(&durations, 0.9),
        max_secs: *durations.last().unwrap_or(&0),
    });

    // Counted over every event, including kinds that open no hold: the
    // question is where pact was RUN, not where work was held.
    let mut by_invoked_from: BTreeMap<String, usize> = BTreeMap::new();
    for (_, e) in &events {
        let key = e
            .invoked_from
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        *by_invoked_from.entry(key).or_insert(0) += 1;
    }
    let mut by_protocol: BTreeMap<String, usize> = BTreeMap::new();
    let mut protocol_unstamped = 0usize;
    let mut by_pact_version: BTreeMap<String, usize> = BTreeMap::new();
    for (_, e) in &events {
        match &e.protocol_hash {
            Some(h) => *by_protocol.entry(h.clone()).or_insert(0) += 1,
            None => protocol_unstamped += 1,
        }
        if let Some(v) = &e.pact_version {
            *by_pact_version.entry(v.clone()).or_insert(0) += 1;
        }
    }
    let watches_active = crate::watch::active(repo_root)
        .map(|w| w.len())
        .unwrap_or(0);
    let diffs_delivered = by_kind.get("notified").copied().unwrap_or(0);
    let deliveries_failed = by_kind.get("watch-delivery-failed").copied().unwrap_or(0);
    let ctx = crate::repo::RepoContext::resolve(repo_root);
    let from_a_worktree = by_invoked_from
        .keys()
        .any(|k| k != "main" && k != "outside" && k != "unknown");
    // Only when the evidence is unambiguous: worktrees exist NOW, events
    // exist, at least one of them was stamped at all, and none names a
    // worktree. Requiring a stamped event is what keeps a pre-stamping log
    // from producing a false hint — see `Summary::topology_note`.
    let any_stamped = by_invoked_from.keys().any(|k| k != "unknown");
    let topology_note = (ctx.has_worktrees && any_stamped && !from_a_worktree).then(|| {
        "this repository has linked worktrees, but no event was invoked from one — agents may \
         be editing in worktrees while running pact from the main checkout, in which case the \
         lease/edit binding rests on convention and cannot be verified from this log"
            .to_string()
    });

    Ok(Summary {
        context: loaded.context,
        events: events.len(),
        contention: contention_stats(&events),
        excluded_by_annotation: loaded.excluded,
        annotations: loaded.annotations,
        unparseable_lines: unparseable,
        orphaned_closes,
        steals: forced_total,
        reclaims: reclaim_total,
        by_kind,
        agents: agents.into_iter().collect(),
        first_event_at: events.first().map(|(_, e)| e.at.clone()),
        last_event_at: events.last().map(|(_, e)| e.at.clone()),
        open_holds: holds.iter().filter(|h| h.closed_at.is_none()).count(),
        hold_secs,
        top_contended,
        per_agent,
        handoff_coverage: handoff_coverage(repo_root),
        models_by_events,
        model_undeclared,
        by_invoked_from,
        by_protocol,
        protocol_unstamped,
        by_pact_version,
        watches_active,
        diffs_delivered,
        deliveries_failed,
        topology_note,
    })
}

/// Refusals related to claims, and the pairs that never got what they asked for.
///
/// Always reported, zeroes included — the same shape as the `steals`/`reclaims`
/// counts beside it, and for the same reason: a run that genuinely did not contend
/// is a measurement, not a gap. megablast recorded zero refusals across 20 agents
/// with the instrumentation compiled in, and that zero is what
/// docs/fleet-patterns.md cites as evidence that wave scheduling pre-resolves
/// contention.
///
/// The one thing it cannot distinguish is a log written before the `refused` kind
/// existed at all, which also reads as zero. `pact_version` on the events is how to
/// tell, and docs/audit.md says so — making the whole struct optional to encode it
/// would have cost `--compare` the ability to track the ratio, which is the only
/// form in which these numbers mean anything.
fn contention_stats(events: &[(usize, Event)]) -> Contention {
    let refusals: Vec<(&str, &str)> = events
        .iter()
        .filter(|(_, e)| e.kind == "refused")
        .filter_map(|(_, e)| e.path.as_deref().map(|p| (e.agent.as_str(), p)))
        .collect();
    let claimed: BTreeSet<(&str, &str)> = events
        .iter()
        .filter(|(_, e)| opens(&e.kind))
        .filter_map(|(_, e)| e.path.as_deref().map(|p| (e.agent.as_str(), p)))
        .collect();
    let pairs: BTreeSet<(&str, &str)> = refusals.iter().copied().collect();
    let abandoned: BTreeSet<(&str, &str)> = pairs.difference(&claimed).copied().collect();
    Contention {
        refusals: refusals.len(),
        claims: events.iter().filter(|(_, e)| opens(&e.kind)).count(),
        contended_pairs: pairs.len(),
        abandoned_pairs: abandoned.len(),
        abandoned_refusals: refusals.iter().filter(|p| abandoned.contains(p)).count(),
    }
}

pub fn render_summary(s: &Summary) -> String {
    if s.events == 0 {
        // Context rows are not behaviour, so a run that has declared its policy
        // and not yet done anything lands here. Say what was declared rather
        // than only that nothing happened: "constraints recorded, no behaviour
        // yet" is a different state from "nothing here at all", and the operator
        // who just ran `pact context set` is checking precisely that it landed.
        let declared = if s.context.is_empty() {
            String::new()
        } else {
            let pairs: Vec<String> = s.context.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("\n  context  {}", pairs.join("  "))
        };
        return format!(
            "no coordination history yet{}{declared}\n\n\
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
    // Before the numbers, not after: a reader who has already formed a story
    // from the statistics is the reader this line exists to reach first.
    if s.context.is_empty() {
        out.push(
            "  context  none recorded — behaviour here cannot be told apart from instruction \
             (`pact context set`, docs/audit.md)"
                .to_string(),
        );
    } else {
        let pairs: Vec<String> = s.context.iter().map(|(k, v)| format!("{k}={v}")).collect();
        out.push(format!("  context  {}", pairs.join("  ")));
    }
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
    if !s.by_invoked_from.is_empty() {
        // `unknown` last and spelled out: a log that predates context
        // stamping must read as "not recorded", never as a topology.
        let mut parts: Vec<String> = s
            .by_invoked_from
            .iter()
            .filter(|(k, _)| k.as_str() != "unknown")
            .map(|(k, n)| format!("{n} from {k}"))
            .collect();
        if let Some(n) = s.by_invoked_from.get("unknown") {
            parts.push(format!("{n} predating context stamping"));
        }
        out.push(format!("  run in {}", parts.join(", ")));
    }
    if s.contention.refusals > 0 {
        let c = &s.contention;
        // The ratio, not just the count: a refusal total on its own cannot
        // distinguish contention that resolved from a fleet thrashing. Printed only
        // when something was refused — a line reading "0 refusals" on every quiet
        // repo is noise, and --json carries the zero for anything that wants it.
        let per_claim = if c.claims > 0 {
            format!(
                "{:.1} per successful claim",
                c.refusals as f64 / c.claims as f64
            )
        } else {
            "no claim ever landed".to_string()
        };
        out.push(format!(
            "  conten {} refusal(s), {per_claim}{}",
            c.refusals,
            if c.abandoned_pairs > 0 {
                format!(
                    "; {} path(s) refused and never acquired ({} refusal(s) abandoned)",
                    c.abandoned_pairs, c.abandoned_refusals
                )
            } else {
                String::new()
            }
        ));
    }
    if s.watches_active > 0 || s.diffs_delivered > 0 || s.deliveries_failed > 0 {
        out.push(format!(
            "  watch  {} active; {} diff(s) delivered{}",
            s.watches_active,
            s.diffs_delivered,
            if s.deliveries_failed > 0 {
                format!(", {} delivery FAILED", s.deliveries_failed)
            } else {
                String::new()
            }
        ));
    }
    // Two or more KNOWN hashes, never a known one plus "not recorded". The
    // first version counted unstamped events as an era, so grimcast — one
    // unchanged block, read across a mid-run upgrade from pact 0.7.2 (which
    // could not stamp) to 0.7.4 (which could) — was told its protocol had
    // changed. It had not: AGENTS.md was last written 23 minutes before the
    // run's first event and never touched again. The reader believed the
    // false version until git was consulted, which is the whole cost of
    // getting this line wrong.
    if s.by_protocol.len() > 1 {
        let mut parts: Vec<String> = s
            .by_protocol
            .iter()
            .map(|(h, n)| format!("{n} under {h}"))
            .collect();
        parts.sort();
        out.push(format!(
            "  proto  the protocol block CHANGED inside this window: {}",
            parts.join(", ")
        ));
    }
    if s.protocol_unstamped > 0 {
        out.push(format!(
            "  proto  {} event(s) predate protocol stamping — which protocol they were written \
             under is not recorded",
            s.protocol_unstamped
        ));
    }
    // The thing that DID change in that run, and which nothing reported. A
    // newer binary can record fields an older one could not, so a field that
    // appears halfway through a log is explained here rather than left to
    // look like the fleet changed its behaviour.
    if s.by_pact_version.len() > 1 {
        let mut parts: Vec<String> = s
            .by_pact_version
            .iter()
            .map(|(v, n)| format!("{n} by {v}"))
            .collect();
        parts.sort();
        out.push(format!(
            "  pact   the binary was UPGRADED inside this window: {}",
            parts.join(", ")
        ));
    }
    if let Some(note) = &s.topology_note {
        out.push(format!("  note   {note}"));
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
        // Said out loud, because "completed" silently excludes it. `open_holds` has
        // been computed since this summary existed and was never rendered, so a run
        // that ended holding something reported hold statistics with a hole in them
        // and nothing pointing at the hole — the failure `excluded_by_annotation` and
        // `orphaned_closes` are both here to prevent.
        //
        // Deliberately NOT phrased as a leak. An offline tool cannot tell "the run
        // ended badly" from "the run is still going", and the author of this line
        // made exactly that mistake reading a live fleet's log: a hold three minutes
        // into a 45-minute TTL is an agent working. State the fact, name the TTL, let
        // the reader judge.
        if s.open_holds > 0 {
            out.push(format!(
                "  {} hold(s) still open at the end of the log, so NOT in the numbers \
                 above — a fleet still running looks like this too; `pact lease ls` \
                 says whether they are live",
                s.open_holds
            ));
        }
        if h.ended_by_expiry > 0 {
            let short = match h.expiry_short_ttl {
                0 => String::new(),
                n if n == h.ended_by_expiry => format!(
                    " — every one under {}, so these are locks taken to serialize a \
                     quick write, not holds anyone abandoned",
                    secs(SHORT_TTL_SECS)
                ),
                n => format!(
                    " — {n} of them under {}, which is a lock taken to serialize a \
                     quick write rather than a hold anyone abandoned",
                    secs(SHORT_TTL_SECS)
                ),
            };
            out.push(format!(
                "  {} ended by expiry rather than release{short}",
                h.ended_by_expiry
            ));
        }
    }

    // Zero refusals is a RESULT, and the summary could not say so: every contention
    // section simply rendered empty, which reads as "nothing was measured" rather
    // than "nothing happened". Across five field runs the only real contention ever
    // observed was one that had been deliberately engineered, and that finding had
    // nowhere to appear.
    if s.contention.refusals == 0 && s.contention.claims > 0 {
        out.push(String::new());
        out.push(format!(
            "no contention: 0 refusals across {} claim(s) by {} agent(s)",
            s.contention.claims,
            s.agents.len()
        ));
        out.push(
            "  contention was PREVENTED, not resolved — a plan whose waves do not put \
             two agents on one path never reaches the lease. `pact plan lint` checks \
             that before you spawn."
                .to_string(),
        );
    }

    if !s.top_contended.is_empty() {
        out.push(String::new());
        out.push("most contended paths".to_string());
        for c in &s.top_contended {
            out.push(format!(
                "  {:<44} {} hold(s) by {} agent(s){}",
                c.path,
                c.holds,
                c.distinct_agents,
                if c.mutex {
                    "   [mutex, not a file]"
                } else {
                    ""
                }
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
                match (a.steals, a.reclaims) {
                    (0, 0) => String::new(),
                    (0, r) => format!(", {r} reclaim(s)"),
                    (st, 0) => format!(", {st} steal(s)"),
                    (st, r) => format!(", {st} steal(s), {r} reclaim(s)"),
                },
                secs(a.held_secs_total)
            ));
            // Indented under the agent rather than widened into it: the row above
            // is four numbers a reader scans down a column, and threading three
            // free-text fields through it would break that alignment for every
            // fleet, including the majority that declare nothing. Omitted whole
            // when the agent carried no attribution at all.
            let chain = [
                a.harness.as_deref(),
                a.model.as_deref().map(|_| "model"),
                a.branch.as_deref().map(|_| "branch"),
            ];
            if chain.iter().any(Option::is_some) {
                let mut parts = Vec::new();
                if let Some(h) = &a.harness {
                    parts.push(h.clone());
                }
                // "declared" is not decoration. Everywhere else in this report a
                // value was measured from the log; this one was asserted by
                // whoever launched the agent, and a reader comparing models
                // across a fleet has to know which kind of claim they are
                // reading. docs/audit.md carries the same warning at length.
                if let Some(m) = &a.model {
                    parts.push(format!("model {m} (declared)"));
                }
                if let Some(b) = &a.branch {
                    parts.push(format!("branch {b}"));
                }
                out.push(format!("  {:<24} {}", "", parts.join(", ")));
            }
        }
    }

    // One line, and only when there is more than nothing to say. The question it
    // answers is "was this run the fleet I think it was" — a run that was meant
    // to be uniform and shows two models is orchestration drift, and a run that
    // shows none has not started declaring yet.
    //
    // By EVENTS, not by agent: an agent that acquired once and one that ran the
    // whole build are not equal evidence about what the run was made of, and
    // counting heads would say they were.
    // Coverage, stated as a fraction and never as a verdict. A bead with nothing
    // worth saying should send nothing, so this is where that shows up rather than
    // a rule anybody broke — which is also why it is here, in the summary that
    // judges nothing, and not in a `--check` that always does.
    if let Some(c) = &s.handoff_coverage {
        if c.with_dependents > 0 {
            out.push(String::new());
            out.push(format!(
                "handoff coverage  {} of {} bead(s) with dependents left findings",
                c.handed_off, c.with_dependents
            ));
            if !c.silent.is_empty() {
                out.push(format!("  silent: {}", c.silent.join(", ")));
                out.push(
                    "  (a smell, not a failure — a bead with nothing worth saying should \
                     send nothing)"
                        .to_string(),
                );
            }
        }
    }

    if !s.models_by_events.is_empty() {
        let mut by_events: Vec<_> = s.models_by_events.iter().collect();
        by_events.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let mut line = by_events
            .iter()
            .map(|(m, n)| format!("{m} {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let undeclared = s.model_undeclared;
        // The undeclared count is the load-bearing half. Without it a run where
        // one agent declared and nineteen did not reads as a single-model fleet,
        // which is the exact wrong conclusion.
        if undeclared > 0 {
            line.push_str(&format!(", undeclared {undeclared}"));
        }
        out.push(String::new());
        out.push(format!("models by events (declared)  {line}"));
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::fixtures::*;

    /// pact-1gv.4: the ratio, and the pairs that never got what they asked for.
    #[test]
    fn the_summary_relates_refusals_to_what_they_bought() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "a", "acquired", "p.rs"),
            &ev_refused("2026-08-11T08:00:10Z", "b", "p.rs", "a", 100),
            &ev("2026-08-11T08:01:00Z", "a", "released", "p.rs"),
            // b eventually got it: contention that resolved.
            &ev("2026-08-11T08:01:01Z", "b", "acquired", "p.rs"),
            // c asked twice for a path it never held: abandoned.
            &ev_refused("2026-08-11T08:01:10Z", "c", "p.rs", "b", 90),
            &ev_refused("2026-08-11T08:01:20Z", "c", "p.rs", "b", 80),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        let c = &s.contention;
        assert_eq!(c.refusals, 3);
        assert_eq!(
            c.claims, 2,
            "a's acquire and b's, the only two claims that landed"
        );
        assert_eq!(c.contended_pairs, 2, "(b,p.rs) and (c,p.rs)");
        assert_eq!(c.abandoned_pairs, 1, "only c never got it");
        assert_eq!(c.abandoned_refusals, 2);

        let text = render_summary(&s);
        assert!(text.contains("3 refusal(s)"), "{text}");
        assert!(text.contains("per successful claim"), "{text}");
        assert!(
            text.contains("1 path(s) refused and never acquired"),
            "{text}"
        );
    }

    /// A quiet run must not print a contention line at all — "0 refusals" on every
    /// repo that never contended is noise. The zero still rides in --json so
    /// --compare can track it.
    #[test]
    fn a_run_with_no_refusals_reports_zero_without_a_line() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "a", "acquired", "p.rs"),
            &ev("2026-08-11T08:01:00Z", "a", "released", "p.rs"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.contention.refusals, 0);
        assert_eq!(s.contention.abandoned_pairs, 0);
        assert!(
            !render_summary(&s).contains("refusal(s)"),
            "{}",
            render_summary(&s)
        );
    }

    #[test]
    fn the_summary_counts_events_by_where_pact_was_invoked() {
        let tmp = with_log(&[
            &ev_from("2026-08-01T10:00:00Z", "a", "acquired", "a.rs", "main"),
            &ev_from("2026-08-01T10:01:00Z", "a", "released", "a.rs", "main"),
            &ev_from("2026-08-01T10:02:00Z", "b", "acquired", "b.rs", "wt-b"),
            &ev_from("2026-08-01T10:03:00Z", "b", "released", "b.rs", "wt-b"),
            &ev_from("2026-08-01T10:04:00Z", "c", "acquired", "c.rs", "outside"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.by_invoked_from.get("main"), Some(&2));
        assert_eq!(s.by_invoked_from.get("wt-b"), Some(&2));
        assert_eq!(s.by_invoked_from.get("outside"), Some(&1));
        assert_eq!(s.by_invoked_from.get("unknown"), None);

        let text = render_summary(&s);
        assert!(text.contains("2 from main"), "{text}");
        assert!(text.contains("2 from wt-b"), "{text}");
    }

    /// The whole-log-predates-stamping case, which every existing repository
    /// is. It must read as "not recorded" and must never produce a topology
    /// claim or a hint — the "no data before <date>" convention.
    #[test]
    fn a_pre_stamping_log_reports_unknown_and_never_hints() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.by_invoked_from.get("unknown"), Some(&2));
        assert_eq!(s.by_invoked_from.len(), 1, "{:?}", s.by_invoked_from);
        assert_eq!(
            s.topology_note, None,
            "a log that predates stamping says nothing about topology"
        );

        let text = render_summary(&s);
        assert!(text.contains("predating context stamping"), "{text}");
        // Must not claim a topology it cannot know.
        assert!(!text.contains("from main"), "{text}");
    }

    /// pact-b73.1, from the field: grimcast spanned an upgrade from a pact
    /// that could not stamp the protocol to one that could, under ONE
    /// unchanged block — and was told its protocol had changed.
    #[test]
    fn a_run_spanning_the_stamps_introduction_is_not_a_protocol_change() {
        let tmp = with_log(&[
            &ev_meta(
                "2026-08-01T10:00:00Z",
                "a",
                "acquired",
                "a.rs",
                r#","pact_version":"0.7.2""#,
            ),
            &ev_meta(
                "2026-08-01T10:01:00Z",
                "a",
                "released",
                "a.rs",
                r#","pact_version":"0.7.2""#,
            ),
            &ev_meta(
                "2026-08-01T10:02:00Z",
                "b",
                "acquired",
                "b.rs",
                r#","pact_version":"0.7.4","protocol_hash":"97b43b5d""#,
            ),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.protocol_unstamped, 2);
        assert_eq!(
            s.by_protocol.len(),
            1,
            "one KNOWN protocol: {:?}",
            s.by_protocol
        );

        let text = render_summary(&s);
        assert!(
            !text.contains("protocol block CHANGED"),
            "unstamped events are not an era: {text}"
        );
        assert!(text.contains("predate protocol stamping"), "{text}");
        // And the thing that actually changed is reported.
        assert!(text.contains("binary was UPGRADED"), "{text}");
        assert!(text.contains("0.7.2") && text.contains("0.7.4"), "{text}");
    }

    /// Two genuinely different blocks must still warn — the fix must not
    /// silence the case the line exists for.
    #[test]
    fn two_known_protocol_hashes_still_report_a_change() {
        let tmp = with_log(&[
            &ev_meta(
                "2026-08-01T10:00:00Z",
                "a",
                "acquired",
                "a.rs",
                r#","protocol_hash":"aaaaaaaa""#,
            ),
            &ev_meta(
                "2026-08-01T10:01:00Z",
                "b",
                "acquired",
                "b.rs",
                r#","protocol_hash":"bbbbbbbb""#,
            ),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.by_protocol.len(), 2);
        assert_eq!(s.protocol_unstamped, 0);
        assert!(render_summary(&s).contains("protocol block CHANGED"));
    }

    /// One binary for the whole run is the normal case and earns no line.
    #[test]
    fn a_single_pact_version_is_not_worth_reporting() {
        let tmp = with_log(&[
            &ev_meta(
                "2026-08-01T10:00:00Z",
                "a",
                "acquired",
                "a.rs",
                r#","pact_version":"0.7.4""#,
            ),
            &ev_meta(
                "2026-08-01T10:01:00Z",
                "a",
                "released",
                "a.rs",
                r#","pact_version":"0.7.4""#,
            ),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.by_pact_version.len(), 1);
        assert!(!render_summary(&s).contains("UPGRADED"));
    }
}
