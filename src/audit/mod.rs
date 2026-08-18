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
//! Audit never opens a Beads *database* — no Dolt directory, no SQLite file —
//! because "pact never touches the Beads store directly" is an invariant the
//! whole messaging design rests on, and an analytics command is exactly where
//! it would be convenient to break it. The one `.beads/` artifact it reads is
//! the committed, append-only `interactions.jsonl` export
//! ([`crate::beads::interaction_assignees`], pact-as5.6), read-only and
//! parse-tolerant — the same shape of file as `.pact/events.jsonl` on pact's
//! own side, and the reason audit needs no subprocess. Richer Beads-side
//! questions live in `scripts/beads-retro.sh`, which is best-effort, jq-based,
//! and says so in its header.
//!
//! `.pact/events.jsonl` is not the *only* thing audit reads, though — as of
//! `Check::CommitCorrelation` (pact-1l8.1) it also shells out to `git log`
//! (`git_history.rs`), the same way `repo.rs` and `doctor.rs` already do for
//! other checks. That is not the same invariant as the Beads one above: `git`
//! is a hard requirement of running pact at all, not a store pact promises to
//! only ever touch through an indirection layer, so reading its history
//! directly breaks nothing that invariant protects.
//!
//! No new dependencies otherwise: line-by-line `serde_json` over the log,
//! tolerant of unknown event kinds (a `kind` is a `String`, so a future one
//! parses) and of a truncated final line (an append-only log gets cut
//! mid-write, which is expected rather than corrupt — the count is
//! reported).
//!
//! ## Exit codes
//!
//! `0` clean, `1` findings — the documented contract, reused rather than
//! extended. An audit finding is not a usage error, so it must not be 5, and it
//! is not a lease conflict, so it must not be 2.

mod context;
mod export;
#[cfg(test)]
mod fixtures;
mod model;
mod summary;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::events::{ChainMismatch, Event};
use crate::lease::{ttl_as_i64, DEFAULT_TTL_SECS};
use context::load;
use model::{
    closes, is_injector, opens, parse_at, reconstruct, DoubleWin, Hold, LEGACY_DEFAULT_TTL_SECS,
};

pub use context::parse_since;
pub use export::{compare, export, render_comparison};
pub use summary::{render_summary, summary};

/// Which named check to run. Absent means the summary.
///
/// Not `Copy` since `Topology` carries `Expect`, which carries its exception list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    DoubleWin,
    StaleHolds,
    /// pact-m7j.2.5: does every chain-tracked line's `chain_hash` match what
    /// it should be, given the line before it? Separate from the other two
    /// checks on purpose — this one is about the log's own physical
    /// integrity, not about lease behaviour, and a line with no `chain_hash`
    /// is not a finding here (see `Event::chain_hash`'s doc comment).
    ChainIntegrity,
    /// pact-1l8.1: does real git history back up what the lease log claims?
    /// Widens audit's stated "`.pact/` and nothing else" scope for the first
    /// time — deliberately: the invariant that section actually protects is
    /// "never touch the Beads store directly, only its CLI", not "never read
    /// anything outside `.pact/`". `git` is already a hard requirement (pact
    /// only runs inside a git repository) and doctor.rs/repo.rs already shell
    /// out to it directly for other checks; this is the same read, applied
    /// to history instead of working-tree state. See `git_history.rs`.
    CommitCorrelation,
    /// pact-ler.2/.5: did this run use the topology it was supposed to?
    /// Carries the expectation, because a check with no declared expectation
    /// has nothing to fail against — the summary already reports the
    /// distribution for a reader who just wants to look.
    Topology(Expect),
    /// pact-mqw.3: did an agent start editing from a copy the previous holder
    /// never produced? A lease is exclusive in TIME; under one-worktree-per-agent
    /// it is not exclusive across COPIES, and the merge that reconciles them is
    /// held by nobody. This is that hazard read back offline, from the content
    /// hashes `acquired` and `released` already carry.
    MergeDivergence,
    /// pact-mqw.4: did a hold's note name a bead that belongs to somebody else?
    /// The one check that reads outside `.pact/` for a Beads-side fact — the
    /// committed `.beads/interactions.jsonl`, never the store, and never a
    /// subprocess. See [`ClaimDivergence`] for the caveat that comes with it, and
    /// [`claim_divergences`] for the sensitivity it trades away.
    ClaimLeaseDivergence,
    /// pact-1gv.3: which agents busy-retried a lease instead of backing off. The
    /// only check about what the FLEET wasted rather than what pact got wrong.
    RetryStorm,
    /// pact-7kv + pact-1gv.7: was a contended path ever communicated about, by
    /// anybody, before its holder let go? Named for what it reports rather than for
    /// its subject, because `Contention` is already the summary's stats struct.
    SilentContention,
}

/// What `--expect` declares a run's topology should have been.
///
/// No longer `Copy`, because `Worktrees` carries its declared exceptions. They belong
/// here rather than as a separate parameter for the same reason the expectation itself
/// does: a declaration with no way to state its exceptions is one nobody can satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// Every stamped event was invoked from a linked worktree, EXCEPT events by these
    /// agents from the main checkout.
    ///
    /// The exception list exists because the check could not pass for any real fleet
    /// (pact-83r.3 / finding 5b). In the topology pact documents, somebody must sit in
    /// the main checkout — it is where the coordination logs are committed from — so an
    /// orchestrator necessarily acts from `main`, and run 5 failed this check with 19
    /// offending events, not one of which was an agent working in the wrong place.
    ///
    /// Naming identities rather than a count: "one agent may work from main" would pass
    /// a run where the wrong one did.
    Worktrees { allow_main: Vec<String> },
    /// Every stamped event was invoked from the main checkout.
    Main,
    /// Nothing to fail — report the distribution and exit 0.
    Any,
    /// `--expect` was not passed: take the expectation from the run's own record
    /// (`topology-expectation` in context), and fall back to [`Expect::Any`] when
    /// the run never declared one.
    ///
    /// Resolved at check time rather than at parse time, because the context
    /// lives in the event log and the log has not been read yet when clap builds
    /// the `Check`. A fleet that declared its topology at run start should not
    /// have to repeat it on the command line two hours later — that repetition is
    /// exactly where the declared and the audited drift apart.
    FromContext,
}

impl Expect {
    /// Every value `--expect` accepts, in the order they are documented.
    ///
    /// The single source of truth, exactly as [`Check::NAMES`] is one flag over
    /// (pact-98u): clap renders this into `--help` and refuses anything not on
    /// it, and `parse`'s error below is built from it, so the help, the parser
    /// and the error cannot disagree about what exists.
    ///
    /// [`Expect::FromContext`] is deliberately absent — it is what a *missing*
    /// `--expect` means, not a value anyone can type. It stays out of clap's
    /// possible values for that reason, and out of this list so nothing offers
    /// it.
    pub const NAMES: [&'static str; 3] = ["worktrees", "main", "any"];

    /// `allow_main` names the identities permitted to act from the main checkout, and is
    /// only meaningful for `worktrees` — the other two either have nothing to except or
    /// expect main already.
    ///
    /// Still fallible although clap validates the command line against
    /// [`Expect::NAMES`]: the other caller is the run's own declared
    /// `topology-expectation`, which comes out of the event log and was never
    /// checked by anything.
    pub fn parse(s: &str, allow_main: &[String]) -> Result<Self> {
        match s {
            "worktrees" => Ok(Expect::Worktrees {
                allow_main: allow_main.to_vec(),
            }),
            "main" => Ok(Expect::Main),
            "any" => Ok(Expect::Any),
            other => {
                // Built from NAMES rather than written out, for the same reason
                // `Check::parse`'s is.
                let (last, rest) = Expect::NAMES.split_last().expect("NAMES is non-empty");
                Err(anyhow::anyhow!(
                    "unknown --expect \"{other}\"; expected {} or {last}",
                    rest.join(", ")
                ))
            }
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Expect::Worktrees { .. } => "worktrees",
            Expect::Main => "main",
            Expect::Any => "any",
            // Unreachable by construction: `run_check` resolves `FromContext`
            // into one of the three above before anything reads it. Labelled
            // distinctly rather than aliased to "any" so that if it ever does
            // leak into a report, the report says something odd instead of
            // something plausible and wrong.
            Expect::FromContext => "from-context (unresolved)",
        }
    }

    /// The identities this expectation excuses from the main checkout.
    fn allowed_from_main(&self) -> &[String] {
        match self {
            Expect::Worktrees { allow_main } => allow_main,
            _ => &[],
        }
    }

    /// Is `invoked_from` what this expectation asked for?
    ///
    /// **Every** stamped event must satisfy it — there is no "mostly" and no
    /// proportion threshold. That strictness is what makes the check
    /// meaningful rather than arbitrary: any looser rule needs a cutoff
    /// ("what fraction counts as worktrees?"), and a verdict that depends on a
    /// cutoff nobody derived from data is exactly the failure docs/audit.md
    /// records under the dangling-hash example. All-or-nothing is explainable
    /// in one sentence and cannot drift.
    fn satisfied_by(&self, invoked_from: &str) -> bool {
        match self {
            // Same unreachable case as `label`. Permissive, so a leak can only
            // ever fail to report a violation — never invent one.
            Expect::Any | Expect::FromContext => true,
            Expect::Main => invoked_from == "main",
            // "outside" is not a worktree: it means pact ran somewhere that is
            // not under this repository at all, which is the one value that
            // says the lease/edit binding cannot be assumed.
            Expect::Worktrees { .. } => invoked_from != "main" && invoked_from != "outside",
        }
    }
}

impl Check {
    /// Every check `--check` accepts, in the order they are documented.
    ///
    /// The single source of truth (pact-98u). `--check`'s help text used to be a
    /// hand-written doc comment and had drifted to naming **four** of the nine —
    /// omitting `topology` and `retry-storm`, the two newest and the two a fleet
    /// run most wants, while `--expect`'s own help two options away referred to
    /// "`--check topology`" by name. Anyone picking a check from `--help` would
    /// silently skip more than half of them.
    ///
    /// clap renders this list itself now, so the help cannot say something the
    /// parser does not accept.
    pub const NAMES: [&'static str; 9] = [
        "double-win",
        "stale-holds",
        "chain-integrity",
        "commit-correlation",
        "merge-divergence",
        "claim-lease-divergence",
        "retry-storm",
        "silent-contention",
        "topology",
    ];

    /// The name this check is spelled with on the command line.
    ///
    /// Exhaustive on purpose: adding a variant without adding its name is a
    /// compile error here, and the round-trip test below then fails if it was
    /// not also added to [`Check::NAMES`]. That pair is what keeps the help,
    /// the parser and the enum from drifting apart again.
    pub fn name(&self) -> &'static str {
        match self {
            Check::DoubleWin => "double-win",
            Check::StaleHolds => "stale-holds",
            Check::ChainIntegrity => "chain-integrity",
            Check::CommitCorrelation => "commit-correlation",
            Check::MergeDivergence => "merge-divergence",
            Check::ClaimLeaseDivergence => "claim-lease-divergence",
            Check::RetryStorm => "retry-storm",
            Check::SilentContention => "silent-contention",
            Check::Topology(_) => "topology",
        }
    }

    /// `expect` is only meaningful for `topology`; every other check ignores
    /// it, and clap requires `--check` alongside it so it cannot be passed
    /// alone.
    pub fn parse(s: &str, expect: Option<&str>, allow_main: &[String]) -> Result<Self> {
        match s {
            "double-win" => Ok(Check::DoubleWin),
            "stale-holds" => Ok(Check::StaleHolds),
            "chain-integrity" => Ok(Check::ChainIntegrity),
            "commit-correlation" => Ok(Check::CommitCorrelation),
            "merge-divergence" => Ok(Check::MergeDivergence),
            "claim-lease-divergence" => Ok(Check::ClaimLeaseDivergence),
            "retry-storm" => Ok(Check::RetryStorm),
            "silent-contention" => Ok(Check::SilentContention),
            "topology" => Ok(Check::Topology(match expect {
                Some(e) => Expect::parse(e, allow_main)?,
                // Not `Any` directly: the run may have declared its own
                // expectation, and `FromContext` defers that lookup to check
                // time, where the log has actually been read. It falls back to
                // `Any` when nothing was declared, so `--check topology` alone
                // still means "show me the distribution".
                None => Expect::FromContext,
            })),
            other => {
                // Built from NAMES rather than written out, so this message and
                // the help text cannot disagree about what exists.
                let (last, rest) = Check::NAMES.split_last().expect("NAMES is non-empty");
                Err(anyhow::anyhow!(
                    "unknown check \"{other}\"; expected {} or {last}",
                    rest.join(", ")
                ))
            }
        }
    }
}

/// One path whose next holder started from content the previous holder never
/// produced — the signature of an edit made against a stale copy.
///
/// Both agents were compliant. That is what makes this worth a check of its own:
/// nothing in the lease log looks wrong, because nothing in the lease protocol WAS
/// wrong. The divergence lives between a release on one branch and an acquire on
/// another, in a window no lease covers.
#[derive(Debug, Clone, Serialize)]
pub struct MergeDivergence {
    pub path: String,
    /// Who released, and what they left.
    pub released_by: String,
    pub released_at: String,
    pub released_hash: String,
    /// Who acquired next, and what their copy actually contained.
    pub acquired_by: String,
    pub acquired_at: String,
    pub acquired_hash: String,
    /// 1-based line of the acquire in `.pact/events.jsonl`, which is the only id
    /// an event has (see `events::numbered`).
    pub line: usize,
}

/// Pair each close that recorded a content hash with the next open of the same
/// path that recorded one, and report the pairs that disagree.
///
/// Only *adjacent* pairs, and only in that order: a hash that differs two holds
/// later says nothing, because the intervening holder was entitled to change the
/// file. The claim being made is narrow on purpose — "the copy this agent started
/// from is not the copy the last agent finished with" — and anything wider would
/// flag ordinary sequential work.
///
/// Renewals are neither, and are skipped: a renewal is the same hold continuing,
/// so treating it as a new open would compare an agent against itself.
fn merge_divergences(events: &[(usize, Event)]) -> (Vec<MergeDivergence>, usize) {
    // Per path: the last close's (agent, at, hash), waiting for the next open.
    let mut pending: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    let mut out = Vec::new();
    let mut unhashed = 0usize;
    for (line, e) in events {
        let Some(path) = e.path.as_deref() else {
            continue;
        };
        if closes(&e.kind) {
            match &e.content_hash {
                Some(h) => {
                    pending.insert(path.to_string(), (e.agent.clone(), e.at.clone(), h.clone()));
                }
                // A close with no hash cannot anchor the next comparison, and must
                // also not leave a STALE anchor behind for it — that would compare
                // an acquire against a release two holds back.
                None => {
                    pending.remove(path);
                    unhashed += 1;
                }
            }
            continue;
        }
        if !opens(&e.kind) {
            continue;
        }
        // Consumed either way: one close anchors exactly one comparison, so a
        // third and fourth acquire of an unchanged path do not each report it.
        let Some((released_by, released_at, released_hash)) = pending.remove(path) else {
            continue;
        };
        let Some(acquired_hash) = e.content_hash.clone() else {
            continue;
        };
        if acquired_hash == released_hash {
            continue;
        }
        out.push(MergeDivergence {
            path: path.to_string(),
            released_by,
            released_at,
            released_hash,
            acquired_by: e.agent.clone(),
            acquired_at: e.at.clone(),
            acquired_hash,
            line: *line,
        });
    }
    (out, unhashed)
}

/// One hold whose note named a bead assigned to a different agent.
///
/// **The caveat, stated first because it bounds the claim.** `assignee` is the
/// LAST assignee `.beads/interactions.jsonl` recorded, not the assignee at acquire
/// time — the log records the note, not the bead's state when it was written. So a
/// hold that legitimately handed its bead on afterwards shows up here too. This
/// answers "whose bead did this hold's note name, and who was it last assigned
/// to", which is the retrospective question; the live question is answered at
/// acquire time, where the assignee really is current.
///
/// The second caveat is [`claim_divergences`]': beads never *re*assigned have no
/// row in the export and cannot appear here at all.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimDivergence {
    pub path: String,
    pub agent: String,
    pub bead: String,
    pub assignee: String,
    pub acquired_at: String,
    pub line: usize,
}

/// Cross-check each hold's note against the assignees recorded in
/// `.beads/interactions.jsonl`.
///
/// Widens audit's usual "`.pact/` and nothing else" scope for the second time, the
/// same way [`Check::CommitCorrelation`] widened it for git — and under the same
/// rule: the invariant that section protects is "never touch the Beads DB
/// directly", which this obeys. Since pact-as5.6 it spawns no subprocess at all:
/// [`crate::beads::interaction_assignees`] reads the committed, git-tracked
/// export, so this check runs with no `bd` on `PATH`.
///
/// **It is deliberately less sensitive than the `bd show` version it replaced.**
/// The export records CHANGES, so a bead assigned at creation and never
/// reassigned has no assignee row and resolves to nothing — it finds fewer
/// divergences than a live query would, never more. And bd only writes the export
/// when `audit.enabled` is on, which it is not by default, so on most repositories
/// this check reports "no beads data" and passes; `pact doctor`'s `Beads audit
/// sidecar` check names that and how to switch it on.
///
/// Since pact-as5.5 this is also the ONLY place pact asks the question. The live
/// cross-check that used to run at `lease acquire` time is gone: it spawned `bd
/// show` on the hot path, and measured against this repository's whole event log
/// it would have warned zero times when moved to this offline source (100 acquire
/// notes named a bead, 8 resolved, all 8 to their own acquirer). See
/// [`crate::beads::interaction_assignees`] and `docs/audit.md`.
fn claim_divergences(
    repo_root: &std::path::Path,
    events: &[(usize, Event)],
    report: &mut CheckReport,
) {
    let assignees = crate::beads::interaction_assignees(repo_root);
    if assignees.is_empty() {
        // Absent, empty, unparseable, or no assignee change ever recorded: all
        // one answer, "nothing to check against", and all of them PASS.
        report.claim_unavailable =
            Some("no assignee history in .beads/interactions.jsonl".to_string());
        return;
    }
    // Walked over the OPENING EVENTS rather than over reconstructed `Hold`s,
    // because the note lives on the event's `detail` and a Hold does not carry it.
    // Same set either way — every hold has exactly one open — with no need to widen
    // a serialized shape three other checks render.
    for (line, e) in events.iter().filter(|(_, e)| opens(&e.kind)) {
        let Some(path) = e.path.as_deref() else {
            continue;
        };
        let Some(note) = e.detail.as_deref() else {
            report.holds_naming_no_bead += 1;
            continue;
        };
        let Some(bead) = crate::beads::bead_id_in(note) else {
            report.holds_naming_no_bead += 1;
            continue;
        };
        let Some(assignee) = assignees.get(bead) else {
            continue;
        };
        if assignee == &e.agent {
            continue;
        }
        report.claim_divergences.push(ClaimDivergence {
            path: path.to_string(),
            agent: e.agent.clone(),
            bead: bead.to_string(),
            assignee: assignee.clone(),
            acquired_at: e.at.clone(),
            line: *line,
        });
    }
}

/// One (agent, path) that was refused over and over.
///
/// The crucible shape: agent-02 refused `src/eval.rs` 33 times, and the retry
/// spacing was not adaptive-with-jitter but **15 seconds flat** — 27 of 32 gaps
/// exactly 15, against a median advertised remaining hold of 355 seconds. It
/// retried roughly 24x more often than the number in its own refusal message told
/// it to, and the holder's note, quoted back in every single one, said "LONG BEAD,
/// will renew".
///
/// Nothing here broke a rule, which is why the report says the fleet wasted work
/// rather than that pact was violated.
#[derive(Debug, Clone, Serialize)]
pub struct RetryStorm {
    pub agent: String,
    pub path: String,
    pub refusals: usize,
    /// Seconds between consecutive refusals: the smallest, and the middle one.
    pub min_gap_secs: Option<i64>,
    pub median_gap_secs: Option<i64>,
    /// Median of the holder's remaining lease across those refusals — the number a
    /// rational wait would be keyed to. `None` for logs written before
    /// `holder_remaining_secs` was recorded (pact-1gv.1).
    pub median_holder_remaining_secs: Option<i64>,
    /// Did this agent ever end up holding the path?
    pub ever_claimed: bool,
}

/// How many refusals of one path by one agent stop being contention and start being
/// a poll loop.
///
/// Five, and the number barely matters: the crucible storms were 33, 20, 14, 13 and
/// 8, while ordinary resolved contention in the same log sat at 1. There is a wide
/// empty band between the two, so any threshold in it gives the same answer — which
/// is the argument for having one at all rather than tuning it.
const RETRY_STORM_REFUSALS: usize = 5;

/// A retry is "impatient" when it comes back in less than this fraction of what the
/// holder said it had left.
///
/// A quarter, chosen to be generous rather than tight: an agent that waits a third
/// of the remaining hold is doing something defensible, and the shape this exists
/// to catch is not marginal. agent-02 waited 15 seconds against a median 355 —
/// about a twenty-fourth.
const RETRY_IMPATIENCE_RATIO: i64 = 4;

/// Group refusals by (agent, path) and report the ones that hammered.
///
/// Two independent shapes, either of which flags:
///
/// - **volume** — more than [`RETRY_STORM_REFUSALS`] refusals of one path by one
///   agent. Works on any log, including every one written before the holder's
///   remaining lease was recorded.
/// - **impatience** — median spacing far below the holder's advertised remaining
///   lease. This is the shape that names the actual mistake, and it needs
///   `holder_remaining_secs` (pact-1gv.1): before that field, the number was in
///   English inside `detail`, and a check that regexes its own prose breaks the
///   next time somebody improves the wording.
///
/// Non-agent identities are excluded. `chaos-ghost` is `scripts/chaos.sh` planting
/// a stale lease, and in the crucible log its single failed acquire would otherwise
/// be reported as a badly-behaved peer — the fault injector's deliberate skip
/// counted against the fleet.
fn retry_storms(events: &[(usize, Event)], report: &mut CheckReport) {
    let mut by_pair: BTreeMap<(&str, &str), Vec<&Event>> = BTreeMap::new();
    for (_, e) in events {
        if e.kind != "refused" || is_injector(&e.agent) {
            continue;
        }
        if let Some(path) = e.path.as_deref() {
            by_pair.entry((e.agent.as_str(), path)).or_default().push(e);
        }
        if e.kind == "refused" && e.holder_remaining_secs.is_none() {
            report.refusals_without_remaining += 1;
        }
    }
    let claimed: BTreeSet<(&str, &str)> = events
        .iter()
        .filter(|(_, e)| opens(&e.kind))
        .filter_map(|(_, e)| e.path.as_deref().map(|p| (e.agent.as_str(), p)))
        .collect();

    for ((agent, path), rows) in by_pair {
        let mut gaps: Vec<i64> = rows
            .windows(2)
            .filter_map(|w| {
                let (a, b) = (parse_at(&w[0].at)?, parse_at(&w[1].at)?);
                Some((b - a).num_seconds())
            })
            .collect();
        gaps.sort_unstable();
        let mut remaining: Vec<i64> = rows
            .iter()
            .filter_map(|e| e.holder_remaining_secs)
            .collect();
        remaining.sort_unstable();
        let median = |v: &[i64]| v.get(v.len() / 2).copied();
        let median_gap = median(&gaps);
        let median_remaining = median(&remaining);

        let by_volume = rows.len() > RETRY_STORM_REFUSALS;
        // Only when both numbers exist. A missing remaining-lease is scope, not
        // evidence of patience.
        let by_impatience = match (median_gap, median_remaining) {
            (Some(gap), Some(rem)) => rem > 0 && gap * RETRY_IMPATIENCE_RATIO < rem,
            _ => false,
        };
        if !by_volume && !by_impatience {
            continue;
        }
        report.retry_storms.push(RetryStorm {
            agent: agent.to_string(),
            path: path.to_string(),
            refusals: rows.len(),
            min_gap_secs: gaps.first().copied(),
            median_gap_secs: median_gap,
            median_holder_remaining_secs: median_remaining,
            ever_claimed: claimed.contains(&(agent, path)),
        });
    }
    // Worst first: the loudest offender is what a reader wants at the top.
    report
        .retry_storms
        .sort_by_key(|r| std::cmp::Reverse(r.refusals));
}

/// A path somebody was refused, whose holder then released it without a word.
///
/// **The threshold objection that deferred pact-7kv dissolves once the boundary is
/// the HOLD.** The check was parked because "communicated in the same window" needed
/// an arbitrary cutoff, and it does not: a refusal happens while some agent holds the
/// path, `reconstruct` already computes that hold exactly, and the question becomes
/// "between the refusal and that holder's release, did anything communicate about
/// this path". No cutoff, nothing to tune — the same all-or-nothing move that
/// unblocked pact-ler.5.
///
/// Three things count as communication, and the third is the one this run forced:
///
/// 1. a `notified` delivery for the path — the holder's release told a subscriber;
/// 2. a message tagged with the path (the `message_id` on a `notified`, or a
///    `--to-owner-of` send, both of which land as events);
/// 3. **the refused agent already held a covering watch at the moment of the
///    refusal** — it had arranged to be told, which is using the channel that works.
///
/// (3) is counted but deliberately does NOT net out of the contention numbers.
/// 24 of the crucible run's 124 refusals came from an agent that was already
/// subscribed, and those same agents then polled 13, 6 and 3 more times. Crediting
/// the subscription while the agent busy-retried would score the run as communicating
/// well at exactly the moment it wasted the most work — so `refusals_with_a_channel`
/// is reported alongside, and `--check retry-storm` still says what the agent did
/// with the channel it had.
#[derive(Debug, Clone, Serialize)]
pub struct SilentContention {
    pub path: String,
    /// Who was refused, and when.
    pub refused_agent: String,
    pub refused_at: String,
    pub line: usize,
    /// Who was holding it, and when they let go. `None` when the log never shows the
    /// hold closing — an open hold has not had its chance to communicate yet, so
    /// those are skipped rather than reported.
    pub holder: String,
    pub holder_released_at: String,
    /// Was the refused agent subscribed to the path at the time? Reported per
    /// finding, because "nobody said anything AND they had no channel either" is a
    /// different situation from "nobody said anything but they were subscribed".
    pub refused_agent_had_a_channel: bool,
}

/// Refusals whose holder released without anything communicating about the path.
fn silent_contentions(
    repo_root: &std::path::Path,
    events: &[(usize, Event)],
    holds: &[Hold],
    report: &mut CheckReport,
) {
    // Point-in-time, not the live registry: a subscription retired afterwards must
    // not rewrite whether the agent had a channel at the refusal (pact-1gv.7).
    let (watch_records, _) = crate::watch::records(repo_root).unwrap_or_default();

    for (line, e) in events.iter().filter(|(_, e)| e.kind == "refused") {
        let Some(path) = e.path.as_deref() else {
            continue;
        };
        if is_injector(&e.agent) {
            continue;
        }
        let had_channel = crate::watch::was_subscribed_at(&watch_records, &e.agent, path, &e.at);
        if had_channel {
            report.refusals_with_a_channel += 1;
        }
        let Some(refused_at) = parse_at(&e.at) else {
            continue;
        };
        // The hold this refusal collided with: same path, spanning the refusal, held
        // by somebody else. That hold's close is the deadline to communicate by.
        let Some(hold) = holds.iter().find(|h| {
            h.path == path
                && h.agent != e.agent
                && parse_at(&h.opened_at).is_some_and(|o| o <= refused_at)
                && h.closed_at
                    .as_deref()
                    .and_then(parse_at)
                    .is_some_and(|c| c >= refused_at)
        }) else {
            // No closed hold covering it. Either the log does not show the hold
            // closing — it has not had its chance yet — or the refusal collided with
            // something reconstruct could not pair. Neither is a finding.
            continue;
        };
        let Some(released_at) = hold.closed_at.as_deref().and_then(parse_at) else {
            continue;
        };
        // Did anything at all communicate about this path in that window?
        let communicated = events.iter().any(|(_, c)| {
            c.path.as_deref() == Some(path)
                && (c.kind == "notified" || c.message_id.is_some())
                && parse_at(&c.at).is_some_and(|t| t >= refused_at && t <= released_at)
        });
        if communicated || had_channel {
            continue;
        }
        report.silent_contentions.push(SilentContention {
            path: path.to_string(),
            refused_agent: e.agent.clone(),
            refused_at: e.at.clone(),
            line: *line,
            holder: hold.agent.clone(),
            holder_released_at: hold.closed_at.clone().unwrap_or_default(),
            refused_agent_had_a_channel: had_channel,
        });
    }
}

/// One invocation point that contradicted `--expect`.
#[derive(Debug, Clone, Serialize)]
pub struct TopologyMismatch {
    pub invoked_from: String,
    pub events: usize,
}

/// `Check::CommitCorrelation`: a closed hold with no commit landing anywhere
/// inside its own window.
///
/// Informational, never a finding that fails the check — see
/// `CheckReport::findings`'s doc comment. A read-only lease (research,
/// reviewing, waiting on something) closes exactly like this, and treating
/// every one as a defect would train readers to ignore the check.
#[derive(Debug, Clone, Serialize)]
pub struct UncommittedHold {
    pub path: String,
    pub agent: String,
    pub opened_at: String,
    pub closed_at: Option<String>,
}

/// `Check::CommitCorrelation`: one commit that landed while a path was held.
#[derive(Debug, Clone, Serialize)]
pub struct CommitTouch {
    pub hash: String,
    pub author: String,
    pub at: String,
}

/// `Check::CommitCorrelation`: two holds of the same path with overlapping
/// windows where real commits — not just the lease events — actually landed
/// during the overlap.
///
/// Stronger evidence than `DoubleWin`, which only proves the *lease* events
/// overlapped. This proves work was actually written more than once during
/// the disputed period. Deliberately does not try to attribute which commit
/// belongs to which hold by matching commit author against agent name — that
/// correlation is exactly as unreliable as the one `doctor::checks`'s "Beads
/// actor attribution" check exists to flag (a shared checkout collapses every
/// agent's commits to one git identity) — so this reports every commit found
/// in the overlap and lets a human attribute them.
#[derive(Debug, Clone, Serialize)]
pub struct ConcurrentWrite {
    pub path: String,
    pub first_agent: String,
    pub first_opened_at: String,
    pub first_closed_at: String,
    pub second_agent: String,
    pub second_opened_at: String,
    pub second_closed_at: String,
    pub overlap_start: String,
    pub overlap_end: String,
    /// Always 2 or more — that is what makes this "concurrent" rather than
    /// merely "both holds happened to eventually touch the file".
    pub commits_in_overlap: Vec<CommitTouch>,
}

/// `Check::CommitCorrelation`: a commit touching a path with no hold covering
/// its author date at all — work done with no lease, the thing the whole
/// protocol exists to prevent. Scoped to paths that were leased at *some*
/// point in the window audited; a path nobody has ever leased is a different
/// question ("is this file even under pact's protocol") that this check does
/// not try to answer, since most of a real repository is never leased at any
/// given moment and flagging all of it would be pure noise.
#[derive(Debug, Clone, Serialize)]
pub struct UncoveredCommit {
    pub path: String,
    pub hash: String,
    pub author: String,
    pub at: String,
}

/// `Check::CommitCorrelation`: a commit that fell inside a hold, but a hold held
/// by a DIFFERENT agent than the one that made the commit (pact-mqw.10).
///
/// The class that had to exist because "covered" was answering the wrong question.
/// `uncovered` asks whether ANYONE held the path at that moment; it never asked
/// whether the COMMITTER did. In the crucible run one agent was deliberately told
/// the protocol did not apply to it, authored zero of 346 coordination events, and
/// committed freely — and its **worst** commit was invisible. `6fa4542` touched
/// five files in one unleased shot, and at that instant every one of those paths
/// was under an active lease held by a compliant peer, so the commit passed.
///
/// The perverse consequence: the rogue's most damaging commit was hidden
/// *specifically because* its peers were compliant. The better the rest of the
/// fleet behaves, the better a rogue hides. Cost of that miss: merging the branch
/// conflicted on all five files and could not be resolved by taking either side,
/// because the rogue had branched before three peers' AST variants landed.
/// Detection lag ~13 minutes, and the detector was a human running `git merge`.
///
/// This is a finding, and a louder one than `uncovered`: committing where nobody
/// held the path risks your own work, committing where somebody ELSE held it
/// corrupts theirs.
#[derive(Debug, Clone, Serialize)]
pub struct CrossHeldCommit {
    pub path: String,
    pub hash: String,
    /// The `Pact-Agent` trailer — who actually made this commit.
    pub committer_agent: String,
    /// Who held the path across its author date.
    pub holder: String,
    pub at: String,
    pub hold_opened_at: String,
}

/// The report for a named check: findings plus enough context to judge them.
#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub check: &'static str,
    /// The constraints the run operated under — the same map the summary prints.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub context: std::collections::BTreeMap<String, String>,
    /// `Check::CommitCorrelation` only: `Some(policy)` when the recorded
    /// `commit-policy` means correlating holds against commits answers nothing.
    ///
    /// The distinction this draws is the whole point of the context record.
    /// "No commit was found for 26 holds" and "no agent was permitted to commit"
    /// produce identical event logs, and only the first is a finding. Reported
    /// as its own field rather than as zero findings, because silence is what
    /// let arkanoid-rs's audit read a policy as an emergent workaround.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_policy_skipped: Option<String>,
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
    /// `Check::CommitCorrelation` only: `Some(reason)` when `git log` could
    /// not be read at all — missing binary, or an I/O error running it. The
    /// three commit-based fields below are then always empty, which must
    /// read as "this check could not run", never as "nothing found".
    pub git_unavailable: Option<String>,
    /// `Check::CommitCorrelation` only: holds correlated by their recorded HEAD range
    /// rather than by timestamp (pact-b73.6).
    ///
    /// Reported so the two paths are never confused. A range answers exactly what a
    /// hold landed; a timestamp window infers it, and on a busy fleet a commit by
    /// somebody else can fall inside your window. Both numbers are printed whenever
    /// either is non-zero, because "this check got more precise for 133 of 153 holds"
    /// is the kind of change a reader must be able to see rather than deduce.
    #[serde(default)]
    pub correlated_by_head: usize,
    /// Holds that fell back to the timestamp window: no head recorded (every log
    /// written before pact stamped it), or a recorded hash that no longer resolves.
    #[serde(default)]
    pub correlated_by_time: usize,
    /// `Check::CommitCorrelation` only. See `UncommittedHold`'s doc comment
    /// for why this is informational and excluded from `findings()`.
    pub holds_with_no_commit: Vec<UncommittedHold>,
    /// `Check::CommitCorrelation` only.
    pub concurrent_writes: Vec<ConcurrentWrite>,
    /// `Check::CommitCorrelation` only.
    pub uncovered_commits: Vec<UncoveredCommit>,
    /// `Check::CommitCorrelation` only (pact-mqw.10): commits made inside somebody
    /// else's hold, as told by their `Pact-Agent` trailer.
    pub cross_held_commits: Vec<CrossHeldCommit>,
    /// `Check::CommitCorrelation` only: how many commits in the window carried a
    /// `Pact-Agent` trailer, and how many did not.
    ///
    /// Both reported, always, because attribution is the difference between this
    /// check answering "did the COMMITTER hold it" and only "did ANYONE hold it".
    /// Every commit made before agents started writing the trailer is in the second
    /// bucket, so a reader has to know which question the numbers answered.
    pub commits_attributed: usize,
    pub commits_unattributed: usize,
    /// `Check::Topology` only: invocation points that contradicted
    /// `--expect`, each with how many events came from it.
    pub topology_mismatches: Vec<TopologyMismatch>,
    /// `Check::Topology` only: what was expected, echoed so a stored report
    /// says what it was judged against rather than only what it found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_topology: Option<&'static str>,
    /// `Check::MergeDivergence` only: successive holds of one path that started
    /// from content the previous holder never left behind.
    pub merge_divergences: Vec<MergeDivergence>,
    /// `Check::MergeDivergence` only: closes carrying no `content_hash`, so the
    /// next acquire had nothing to compare against. Reported, never a finding —
    /// every log written before pact stamped releases is entirely in this state,
    /// and flagging it would fail every existing repository. Same discipline as
    /// `chain_untracked` and `topology_unstamped`.
    pub divergence_unhashed: usize,
    /// `Check::ClaimLeaseDivergence` only.
    pub claim_divergences: Vec<ClaimDivergence>,
    /// `Check::ClaimLeaseDivergence` only: `Some(reason)` when there was no Beads
    /// data to check against at all. `claim_divergences` is then always empty, which
    /// must read as "this check could not run", never as "nothing found" — the same
    /// contract `git_unavailable` has. It is never an error and never a non-zero
    /// exit: a repository with no `.beads/interactions.jsonl` passes.
    pub claim_unavailable: Option<String>,
    /// `Check::ClaimLeaseDivergence` only: holds whose note named no bead, so there
    /// was nothing to cross-check. Scope, not a finding.
    pub holds_naming_no_bead: usize,
    /// `Check::RetryStorm` only.
    pub retry_storms: Vec<RetryStorm>,
    /// `Check::RetryStorm` only: refusals carrying no `holder_remaining_secs`, so
    /// their spacing could not be judged against what the holder advertised.
    /// Reported, never a finding — every log written before pact-1gv.1 is entirely
    /// in this state, and the count-based half of the check still works on them.
    pub refusals_without_remaining: usize,
    /// `Check::Contention` only.
    pub silent_contentions: Vec<SilentContention>,
    /// `Check::Contention` only: refusals where the refused agent already held a
    /// covering watch, so the channel WAS in place. Not a finding, and not netted
    /// out of the count either — see [`SilentContention`].
    pub refusals_with_a_channel: usize,
    /// `Check::Topology` only: events carrying no `invoked_from` at all.
    /// Reported, never a finding — every log written before pact 0.7.0 is
    /// entirely in this state, and flagging it would fail every existing
    /// repository the moment this shipped. Same discipline as
    /// `chain_untracked`.
    pub topology_unstamped: usize,
    /// Events excused by `--allow-main` (pact-83r.3 / finding 5b).
    ///
    /// Counted and reported rather than silently dropped: an exception nobody can see the
    /// size of is an exception that stops being read as one.
    #[serde(default)]
    pub topology_allowed_from_main: usize,
}

impl CheckReport {
    pub fn findings(&self) -> usize {
        self.double_wins.len()
            + self.stale_holds.len()
            + self.chain_breaks.len()
            + self.concurrent_writes.len()
            + self.uncovered_commits.len()
            + self.cross_held_commits.len()
            + self.topology_mismatches.len()
            + self.merge_divergences.len()
            + self.claim_divergences.len()
            + self.retry_storms.len()
            + self.silent_contentions.len()
    }
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
        // `Check::name`, not a second exhaustive match: this one existed
        // alongside the parser and the help text, and the three had already
        // drifted apart once (pact-98u). What a report calls a check, what the
        // parser accepts and what `--help` lists are now one list.
        check: check.name(),
        context: loaded.context.clone(),
        commit_policy_skipped: None,
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
        git_unavailable: None,
        correlated_by_head: 0,
        correlated_by_time: 0,
        holds_with_no_commit: Vec::new(),
        concurrent_writes: Vec::new(),
        uncovered_commits: Vec::new(),
        cross_held_commits: Vec::new(),
        commits_attributed: 0,
        commits_unattributed: 0,
        topology_mismatches: Vec::new(),
        expected_topology: None,
        topology_unstamped: 0,
        topology_allowed_from_main: 0,
        merge_divergences: Vec::new(),
        divergence_unhashed: 0,
        claim_divergences: Vec::new(),
        claim_unavailable: None,
        holds_naming_no_bead: 0,
        retry_storms: Vec::new(),
        refusals_without_remaining: 0,
        silent_contentions: Vec::new(),
        refusals_with_a_channel: 0,
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
        Check::CommitCorrelation => match loaded.context.get("commit-policy").map(String::as_str) {
            // A policy that forbade committing makes the correlation vacuous, and
            // a vacuous check must say so rather than return an empty finding
            // list that reads as a clean bill of health.
            Some(policy @ ("none" | "orchestrator-only")) => {
                report.commit_policy_skipped = Some(policy.to_string());
            }
            _ => correlate_commits(repo_root, &events, &holds, &mut report),
        },
        Check::MergeDivergence => {
            let (divergences, unhashed) = merge_divergences(&events);
            report.merge_divergences = divergences;
            report.divergence_unhashed = unhashed;
        }
        Check::ClaimLeaseDivergence => claim_divergences(repo_root, &events, &mut report),
        Check::RetryStorm => retry_storms(&events, &mut report),
        Check::SilentContention => silent_contentions(repo_root, &events, &holds, &mut report),
        Check::Topology(ref expect) => {
            let resolved;
            let expect = match expect {
                Expect::FromContext => {
                    resolved = match loaded.context.get("topology-expectation") {
                        Some(declared) => Expect::parse(declared, &[]).unwrap_or(Expect::Any),
                        None => Expect::Any,
                    };
                    &resolved
                }
                other => other,
            };
            report.expected_topology = Some(expect.label());
            let allowed = expect.allowed_from_main();
            let mut by_point: BTreeMap<String, usize> = BTreeMap::new();
            for (_, e) in &events {
                match e.invoked_from.as_deref() {
                    // A declared main-checkout identity is excused, and only from `main`
                    // — naming an agent does not license it to act from anywhere else
                    // (pact-83r.3 / finding 5b).
                    Some("main") if allowed.contains(&e.agent) => {
                        report.topology_allowed_from_main += 1;
                    }
                    Some(from) => *by_point.entry(from.to_string()).or_insert(0) += 1,
                    None => report.topology_unstamped += 1,
                }
            }
            report.topology_mismatches = by_point
                .into_iter()
                .filter(|(from, _)| !expect.satisfied_by(from))
                .map(|(invoked_from, events)| TopologyMismatch {
                    invoked_from,
                    events,
                })
                .collect();
        }
    }
    Ok(report)
}

/// The body of `Check::CommitCorrelation`, split out because it is the one
/// check whose findings depend on something other than `.pact/events.jsonl`
/// — see the module doc comment on why that is a deliberate, narrow widening
/// rather than a break of audit's Beads-store invariant.
/// One hold's window plus WHO held it — the second half of which `covered` used to
/// throw away, and the whole of pact-mqw.10's fix.
struct HoldWindow {
    agent: String,
    open: DateTime<Utc>,
    /// `None` while the log shows the lease still held.
    close: Option<DateTime<Utc>>,
}

fn correlate_commits(
    repo_root: &std::path::Path,
    events: &[(usize, Event)],
    holds: &[Hold],
    report: &mut CheckReport,
) {
    let earliest = events.first().and_then(|(_, e)| parse_at(&e.at));
    let commits = match crate::git_history::commits_since(repo_root, earliest) {
        Ok(c) => c,
        Err(e) => {
            report.git_unavailable = Some(format!("{e:#}"));
            return;
        }
    };

    for c in &commits {
        if c.pact_agent.is_some() {
            report.commits_attributed += 1;
        } else {
            report.commits_unattributed += 1;
        }
    }

    let mut by_path: BTreeMap<&str, Vec<&crate::git_history::Commit>> = BTreeMap::new();
    for c in &commits {
        for p in &c.paths {
            by_path.entry(p.as_str()).or_default().push(c);
        }
    }
    for v in by_path.values_mut() {
        v.sort_by_key(|c| c.at);
    }

    // Closed holds only: an open hold has not finished, so judging whether it
    // ever produced a commit would be premature.
    for h in holds {
        let (Some(open), Some(close)) = (
            parse_at(&h.opened_at),
            h.closed_at.as_deref().and_then(parse_at),
        ) else {
            continue;
        };
        // THE EXACT ANSWER, when the log carries it (pact-b73.6). With a head on both
        // boundaries the hold brackets a real commit range, so "did this lease land
        // anything on this path" is a lookup rather than an inference from wall-clock
        // time. Only `git log open..close` can distinguish the agent's own commits
        // from a peer's that merely landed inside the same minutes.
        //
        // The fallback is not optional and never will be: every log written before
        // pact stamped `head` has none, and a recorded hash can stop resolving when a
        // worktree branch is deleted and gc'd, force-pushed, or read in a shallow
        // clone. `commits_in_range` returns `None` for all of those, and the counters
        // above make the choice visible instead of quietly reporting fewer findings.
        let ranged = match (h.open_head.as_deref(), h.close_head.as_deref()) {
            (Some(from), Some(to)) => crate::git_history::commits_in_range(repo_root, from, to),
            _ => None,
        };
        let has_commit = match &ranged {
            Some(paths) => {
                report.correlated_by_head += 1;
                paths.contains(&h.path)
            }
            None => {
                report.correlated_by_time += 1;
                by_path
                    .get(h.path.as_str())
                    .is_some_and(|v| v.iter().any(|c| c.at >= open && c.at <= close))
            }
        };
        if !has_commit {
            report.holds_with_no_commit.push(UncommittedHold {
                path: h.path.clone(),
                agent: h.agent.clone(),
                opened_at: h.opened_at.clone(),
                closed_at: h.closed_at.clone(),
            });
        }
    }

    let mut by_hold_path: BTreeMap<&str, Vec<&Hold>> = BTreeMap::new();
    for h in holds {
        if h.closed_at.is_some() {
            by_hold_path.entry(h.path.as_str()).or_default().push(h);
        }
    }
    for (path, hs) in &by_hold_path {
        for i in 0..hs.len() {
            for j in (i + 1)..hs.len() {
                let (a, b) = (hs[i], hs[j]);
                let (Some(a_open), Some(a_close), Some(b_open), Some(b_close)) = (
                    parse_at(&a.opened_at),
                    a.closed_at.as_deref().and_then(parse_at),
                    parse_at(&b.opened_at),
                    b.closed_at.as_deref().and_then(parse_at),
                ) else {
                    continue;
                };
                let overlap_start = a_open.max(b_open);
                let overlap_end = a_close.min(b_close);
                if overlap_start > overlap_end {
                    continue;
                }
                let commits_in_overlap: Vec<CommitTouch> = by_path
                    .get(path)
                    .map(|v| {
                        v.iter()
                            .filter(|c| c.at >= overlap_start && c.at <= overlap_end)
                            .map(|c| CommitTouch {
                                hash: c.hash.clone(),
                                author: c.author.clone(),
                                at: c.at.to_rfc3339(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Two or more, not one: a single commit in the overlap only
                // proves one hold's tenant wrote once, which the lease system
                // already allows for its own holder. Two is the shape that
                // needs a real write from more than one side.
                if commits_in_overlap.len() >= 2 {
                    report.concurrent_writes.push(ConcurrentWrite {
                        path: path.to_string(),
                        first_agent: a.agent.clone(),
                        first_opened_at: a.opened_at.clone(),
                        first_closed_at: a.closed_at.clone().unwrap_or_default(),
                        second_agent: b.agent.clone(),
                        second_opened_at: b.opened_at.clone(),
                        second_closed_at: b.closed_at.clone().unwrap_or_default(),
                        overlap_start: overlap_start.to_rfc3339(),
                        overlap_end: overlap_end.to_rfc3339(),
                        commits_in_overlap,
                    });
                }
            }
        }
    }

    // Scoped to paths leased at some point in this window — see
    // `UncoveredCommit`'s doc comment for why the rest of the tree is out of
    // scope.
    let leased_paths: BTreeSet<&str> = holds.iter().map(|h| h.path.as_str()).collect();
    for path in &leased_paths {
        let Some(touches) = by_path.get(path) else {
            continue;
        };
        let windows: Vec<HoldWindow> = holds
            .iter()
            .filter(|h| h.path == *path)
            .filter_map(|h| {
                parse_at(&h.opened_at).map(|o| HoldWindow {
                    agent: h.agent.clone(),
                    open: o,
                    close: h.closed_at.as_deref().and_then(parse_at),
                })
            })
            .collect();
        for c in touches {
            // Every hold spanning this commit, with WHO held it — the second half
            // is what `covered` used to throw away.
            let covering: Vec<&HoldWindow> = windows
                .iter()
                .filter(|w| w.open <= c.at && w.close.is_none_or(|cl| cl >= c.at))
                .collect();
            if covering.is_empty() {
                report.uncovered_commits.push(UncoveredCommit {
                    path: path.to_string(),
                    hash: c.hash.clone(),
                    author: c.author.clone(),
                    at: c.at.to_rfc3339(),
                });
                continue;
            }
            // Attribution, or the honest absence of it. Without a trailer this
            // stays exactly as permissive as it was — the alternative would be to
            // guess from `author`, which every fleet so far collapses to one git
            // identity for every agent.
            let Some(committer) = c.pact_agent.as_deref() else {
                continue;
            };
            if covering.iter().any(|w| w.agent == committer) {
                continue;
            }
            // Held by someone, and not by whoever committed. The FIRST covering
            // hold is named rather than all of them: more than one covering hold is
            // itself a double-win, which its own check reports.
            let first = covering[0];
            report.cross_held_commits.push(CrossHeldCommit {
                path: path.to_string(),
                hash: c.hash.clone(),
                committer_agent: committer.to_string(),
                holder: first.agent.clone(),
                at: c.at.to_rfc3339(),
                hold_opened_at: first.open.to_rfc3339(),
            });
        }
    }
    report.uncovered_commits.sort_by(|a, b| a.at.cmp(&b.at));
    report.cross_held_commits.sort_by(|a, b| a.at.cmp(&b.at));
}

// --------------------------------------------------------------- rendering

pub(in crate::audit) fn secs(n: i64) -> String {
    crate::lease::human_secs(n)
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

    if r.check == "topology" {
        // Stated before any verdict, and stated even when clean: a reader has
        // to know how much of the log this check could speak to at all before
        // believing what it says about it.
        out.push(format!(
            "  expected {}; {} event(s) carry no invocation context (written before pact \
             recorded it)",
            r.expected_topology.unwrap_or("any"),
            r.topology_unstamped
        ));
        // In the header for the same reason the line above is: an exception has to be
        // visible on a PASS, or a reader cannot tell "the fleet ran where it was asked"
        // from "the exception list was wide enough to cover where it did not"
        // (pact-83r.3 / finding 5b).
        if r.topology_allowed_from_main > 0 {
            out.push(format!(
                "  {} event(s) excused from the main checkout by --allow-main",
                r.topology_allowed_from_main
            ));
        }
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

    if r.check == "merge-divergence" {
        // Same reasoning as topology's and chain-integrity's scope lines, and
        // stated even when clean: a close with no content hash gives the acquire
        // after it nothing to compare against, so a reader has to know how much
        // of the log this check could speak to before believing what it says.
        out.push(format!(
            "  {} close(s) recorded no content hash, so the acquire after each had nothing to \
             compare against (logs written before pact stamped releases are entirely in this \
             state)",
            r.divergence_unhashed
        ));
    }

    if r.check == "claim-lease-divergence" {
        if let Some(reason) = &r.claim_unavailable {
            // pact-83r.6 / finding 6. "Could not run" on its own sends a reader to bd's
            // `bd audit --help`, which tells them to run `bd config set audit.enabled
            // true` — and bd 1.2.1 answers THAT with `"audit.enabled" is not a
            // recognized config key` before honouring it anyway. Measured end to end:
            // the key works, only bd's config-key allowlist disagrees. A reader who is
            // not told that reasonably concludes the remediation failed and goes hunting
            // for one that does not exist, which is how this check stayed unrun in the
            // field. So the fix is named here, warning and all — `pact audit` never
            // spawns bd, so it cannot check the outcome, only state it accurately.
            out.push(format!(
                "  no beads data ({reason}) — claim-lease-divergence could not run. bd's \
                 audit sidecar is not recording: turn it on with `BD_AUDIT_ENABLED=1` in \
                 the environment your agents run bd in, or `bd config set audit.enabled \
                 true` to persist it — bd 1.2.1 warns that key is unrecognised and then \
                 honours it, so the warning is not a failure. bd records from that point, \
                 not retroactively, so this stays empty for work already done"
            ));
            return out.join("\n");
        }
        out.push(format!(
            "  {} hold(s) named no bead in their note, so there was nothing to cross-check",
            r.holds_naming_no_bead
        ));
    }

    if r.check == "silent-contention" {
        // Stated clean or not. A run where every refused agent was already
        // subscribed has NO findings here, and that fact is the interesting one —
        // silence would read as "nothing to see" rather than "the channel was used".
        out.push(format!(
            "  {} refusal(s) came from an agent already subscribed to the path (channel in \
             place); see --check retry-storm for what they did with it",
            r.refusals_with_a_channel
        ));
    }

    if r.check == "retry-storm" {
        // Scope before verdict, clean or not: the impatience half of this check
        // cannot speak to a refusal whose holder-remaining was never recorded.
        out.push(format!(
            "  {} refusal(s) carry no holder-remaining, so only their COUNT could be \
             judged (logs written before pact recorded it are entirely in this state)",
            r.refusals_without_remaining
        ));
    }

    if r.check == "commit-correlation" {
        // Before the git check: a policy that forbade committing makes the
        // question vacuous whether or not git is readable, and answering
        // "could not run" would blame the wrong thing.
        if let Some(policy) = &r.commit_policy_skipped {
            out.push(format!(
                "  commit policy: {policy} — correlation not evaluated"
            ));
            out.push(
                "  no agent in this run was permitted to commit, so holds without commits are \
                 the policy working, not a finding"
                    .to_string(),
            );
            return out.join("\n");
        }
        if let Some(reason) = &r.git_unavailable {
            out.push(format!(
                "  git history unavailable ({reason}) — commit-correlation could not run"
            ));
            return out.join("\n");
        }
        // Stated clean or not: with no trailers this check can only ask "did ANYONE
        // hold the path", and a reader has to know that is the question it answered.
        out.push(format!(
            "  {} commit(s) carry a Pact-Agent trailer, {} do not{}",
            r.commits_attributed,
            r.commits_unattributed,
            if r.commits_attributed == 0 && r.commits_unattributed > 0 {
                " — with none attributed, a commit counts as covered when ANY agent held the \
                 path, so an agent working without a lease is invisible whenever a compliant peer \
                 holds it (see docs/audit.md)"
            } else {
                ""
            }
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
            "commit-correlation" => {
                "no concurrent write landed, no commit fell outside every hold's window, and \
                 every attributed commit was made by an agent that held the path"
                    .to_string()
            }
            "topology" => format!(
                "every context-stamped event matches --expect {}",
                r.expected_topology.unwrap_or("any")
            ),
            "silent-contention" => {
                "every contended path was communicated about before its holder let go — by a \
                 watch delivery, a message, or the refused agent's own subscription"
                    .to_string()
            }
            "retry-storm" => {
                "no agent hammered a lease it was refused — every retry was either rare or \
                 spaced against what the holder advertised"
                    .to_string()
            }
            "claim-lease-divergence" => {
                "every hold whose note named a bead was held by that bead's own assignee"
                    .to_string()
            }
            "merge-divergence" => {
                "every hold started from the content the previous holder left — no edit was made \
                 against a stale copy"
                    .to_string()
            }
            // `stale-holds` named explicitly rather than left to the catch-all it
            // used to own: a new check landing on that arm inherited the wrong
            // clean message, which is how this comment got written.
            _ => "no holds ran past their own recorded TTL without a renew".to_string(),
        });
        // commit-correlation still has informational rows to print (holds
        // with no commit at all) even with zero fail-worthy findings.
        if r.check != "commit-correlation" {
            return out.join("\n");
        }
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

    for w in &r.concurrent_writes {
        out.push(String::new());
        out.push(format!("CONCURRENT WRITE on {}", w.path));
        out.push(format!(
            "  {} held {} -> {}",
            w.first_agent, w.first_opened_at, w.first_closed_at
        ));
        out.push(format!(
            "  {} held {} -> {}",
            w.second_agent, w.second_opened_at, w.second_closed_at
        ));
        out.push(format!(
            "  {} commit(s) landed in the overlap ({} -> {}):",
            w.commits_in_overlap.len(),
            w.overlap_start,
            w.overlap_end
        ));
        for c in &w.commits_in_overlap {
            out.push(format!(
                "    {} by {} at {}",
                &c.hash[..c.hash.len().min(12)],
                c.author,
                c.at
            ));
        }
    }
    if !r.concurrent_writes.is_empty() {
        out.push(String::new());
        out.push(
            "This is stronger evidence than --check double-win: real commits landed from more\n\
             than one side during the overlap, not just overlapping lease events."
                .to_string(),
        );
    }

    for u in &r.uncovered_commits {
        out.push(String::new());
        out.push(format!(
            "UNCOVERED COMMIT on {}: {} by {} at {} — no hold on this path covered that moment",
            u.path,
            &u.hash[..u.hash.len().min(12)],
            u.author,
            u.at
        ));
    }
    if !r.uncovered_commits.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} commit(s) touched a leased path outside every recorded hold for it — work done \
             with no lease, which the protocol exists to prevent.",
            r.uncovered_commits.len()
        ));
    }

    for x in &r.cross_held_commits {
        out.push(String::new());
        out.push(format!(
            "CROSS-HELD COMMIT on {}: {} by {} at {} — but {} held that path from {}",
            x.path,
            &x.hash[..x.hash.len().min(12)],
            x.committer_agent,
            x.at,
            x.holder,
            x.hold_opened_at
        ));
    }
    if !r.cross_held_commits.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} commit(s) landed on a path a DIFFERENT agent held. Louder than an uncovered \n\
             commit, not quieter: committing where nobody held the path risks your own work, \n\
             committing where somebody else held it corrupts theirs — and a peer's in-flight \n\
             edits are the thing a lease exists to protect.",
            r.cross_held_commits.len()
        ));
    }

    for m in &r.topology_mismatches {
        out.push(String::new());
        out.push(format!(
            "TOPOLOGY MISMATCH: {} event(s) invoked from {:?}, which --expect {} does not allow",
            m.events,
            m.invoked_from,
            r.expected_topology.unwrap_or("any")
        ));
    }
    if !r.topology_mismatches.is_empty() {
        out.push(String::new());
        out.push(
            "The run did not use the topology it was asked to. Under an orchestrated-wave fleet \n\
             this usually means agents edited in their worktrees but ran pact from the main \n\
             checkout, so the lease/edit binding rests on convention — see \n\
             docs/fleet-patterns.md."
                .to_string(),
        );
    }

    for sc in &r.silent_contentions {
        out.push(String::new());
        out.push(format!(
            "SILENT CONTENTION on {}: {} was refused at {} (line {}), and {} released it at {} \
             without a message or a watch delivery about it",
            sc.path, sc.refused_agent, sc.refused_at, sc.line, sc.holder, sc.holder_released_at
        ));
    }
    if !r.silent_contentions.is_empty() {
        out.push(String::new());
        out.push(
            "Somebody wanted a path, was told no, and learned nothing when it came free. The \n\
             cheapest fix is not a message: `pact watch add <path>` makes the next release tell \n\
             them automatically, and delivery rides `lease release`, which agents perform \n\
             reliably. Four fleet runs produced 0 voluntary agent-to-agent messages and 64 watch \n\
             deliveries — see docs/watch.md."
                .to_string(),
        );
    }

    for st in &r.retry_storms {
        out.push(String::new());
        out.push(format!(
            "RETRY STORM: {} refused {} {} time(s){}{}",
            st.agent,
            st.path,
            st.refusals,
            match (st.median_gap_secs, st.median_holder_remaining_secs) {
                (Some(g), Some(rem)) => format!(
                    ", retrying every ~{} against a median {} of holder lease left",
                    secs(g),
                    secs(rem)
                ),
                (Some(g), None) => format!(", retrying every ~{}", secs(g)),
                _ => String::new(),
            },
            if st.ever_claimed {
                String::new()
            } else {
                " — and NEVER got the path".to_string()
            }
        ));
    }
    if !r.retry_storms.is_empty() {
        out.push(String::new());
        out.push(
            "Nothing here broke a rule: a refused agent is entitled to ask again. It is work \n\
             the fleet spent for nothing. The refusal message states how long the holder has \n\
             left; an agent that waits on that order of magnitude — or better, subscribes with \n\
             `pact watch add <path>` and picks up other ready work — spends none of it. One \n\
             fleet retried every 15 seconds, 33 times, against a median 355 seconds of \n\
             remaining hold whose note said it would renew."
                .to_string(),
        );
    }

    for c in &r.claim_divergences {
        out.push(String::new());
        out.push(format!(
            "CLAIM/LEASE DIVERGENCE on {}: held by {} for {}, last assigned to {} (line {}, \
             acquired {})",
            c.path, c.agent, c.bead, c.assignee, c.line, c.acquired_at
        ));
    }
    if !r.claim_divergences.is_empty() {
        out.push(String::new());
        out.push(
            "A fleet on this protocol has TWO mutual-exclusion mechanisms answering two halves \n\
             of one question — `bd update --claim` for who owns the work, `pact lease acquire` \n\
             for who may edit the files — and pact grants the second without consulting the \n\
             first. Assignees above are CURRENT, not as of the acquire, so a bead legitimately \n\
             handed on later appears here too; see docs/audit.md."
                .to_string(),
        );
    }

    for d in &r.merge_divergences {
        out.push(String::new());
        out.push(format!(
            "MERGE DIVERGENCE on {}: {} released it at {} leaving {}, then {} acquired it at {} \
             holding {} instead (line {})",
            d.path,
            d.released_by,
            d.released_at,
            &d.released_hash[..d.released_hash.len().min(12)],
            d.acquired_by,
            d.acquired_at,
            &d.acquired_hash[..d.acquired_hash.len().min(12)],
            d.line
        ));
    }
    if !r.merge_divergences.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} hold(s) started from a copy the previous holder never produced. Both agents were \n\
             compliant — a lease is exclusive in TIME, not across worktrees, so the conflict was \n\
             deferred to a merge no lease covered. git often merges such edits with NO conflict \n\
             marker when they are textually non-adjacent, which is the shape an additive change \n\
             to a shared enum or match statement has. Check those paths for duplicated arms, \n\
             duplicated definitions, or a peer's change silently reverted — see docs/leases.md.",
            r.merge_divergences.len()
        ));
    }
    // How the answer was reached, whenever this check ran at all. A reader must be able
    // to see that a hold was correlated EXACTLY rather than inferred from wall-clock
    // time — and to see when the log is too old to allow it (pact-b73.6).
    if r.correlated_by_head + r.correlated_by_time > 0 {
        out.push(String::new());
        out.push(match (r.correlated_by_head, r.correlated_by_time) {
            (n, 0) => format!(
                "{n} hold(s) correlated by their recorded HEAD range — an exact commit set \
                 per hold, not a timestamp window"
            ),
            (0, n) => format!(
                "{n} hold(s) correlated by TIMESTAMP WINDOW: no usable HEAD range. Logs \
                 written before pact stamped `head` have none, and a recorded hash stops \
                 resolving after a branch is deleted, force-pushed or read in a shallow \
                 clone. A window can attribute a peer's commit to your hold."
            ),
            (h, t) => format!(
                "{h} hold(s) correlated by their recorded HEAD range (exact), {t} by \
                 timestamp window (inferred — no head recorded, or the hash no longer \
                 resolves)"
            ),
        });
    }
    if !r.holds_with_no_commit.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} hold(s) closed with no commit landing in their window (informational, not \
             counted as a finding — a read-only lease produces no commit):",
            r.holds_with_no_commit.len()
        ));
        for h in &r.holds_with_no_commit {
            out.push(format!(
                "  {:<40} {:<16} {} -> {}",
                h.path,
                h.agent,
                h.opened_at,
                h.closed_at.as_deref().unwrap_or("(open)")
            ));
        }
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    /// pact-mqw.3: the crucible shape. Two agents each correctly held one file at
    /// different times, each added a match arm to the SAME match statement on its
    /// own branch, and git merged both insertions cleanly because they were
    /// textually non-adjacent — duplicate arms, no conflict marker, caught by a
    /// later compile failure rather than by pact.
    ///
    /// Nothing in the lease log looks wrong, because nothing in the lease protocol
    /// WAS wrong. The evidence is entirely in the content hashes.
    /// pact-98u. `--check`'s help had drifted to naming four of the nine checks,
    /// omitting `topology` and `retry-storm` — the two a fleet run most wants,
    /// and one of which `--expect`'s own help referred to by name two options
    /// away. This is the guard against it happening again: every name must
    /// parse, and every parsed check must answer with the name it came from.
    #[test]
    fn every_documented_check_name_parses_and_round_trips() {
        for name in Check::NAMES {
            let parsed = Check::parse(name, None, &[])
                .unwrap_or_else(|e| panic!("NAMES lists {name:?} but parse rejects it: {e:#}"));
            assert_eq!(
                parsed.name(),
                name,
                "{name:?} parsed to a check that calls itself something else"
            );
        }
    }

    /// The other direction: a name the parser accepts but NAMES omits would be
    /// invisible in `--help` and unreachable through clap's value parser, which
    /// is exactly the bug. `Check::name` is exhaustive, so a new variant forces
    /// a compile error there; this catches the case where it was named but not
    /// listed.
    #[test]
    fn the_unknown_check_error_names_every_check_that_exists() {
        let err = format!("{:#}", Check::parse("nonsense", None, &[]).unwrap_err());
        for name in Check::NAMES {
            assert!(err.contains(name), "the error omits {name:?}: {err}");
        }
        assert!(err.contains("nonsense"), "{err}");
    }

    /// The `--expect` half of the same guard. `Expect::label` is exhaustive, so
    /// a new variant forces a compile error there; this catches the case where
    /// it was labelled but never added to `NAMES`, which would leave it out of
    /// `--help` and unreachable through clap's value parser — pact-98u exactly.
    #[test]
    fn every_expect_name_parses_and_round_trips() {
        for name in Expect::NAMES {
            let parsed = Expect::parse(name, &[])
                .unwrap_or_else(|e| panic!("NAMES lists {name:?} but parse rejects it: {e:#}"));
            assert_eq!(
                parsed.label(),
                name,
                "{name:?} parsed to an expectation that calls itself something else"
            );
        }
        let err = format!("{:#}", Expect::parse("mostly", &[]).unwrap_err());
        for name in Expect::NAMES {
            assert!(err.contains(name), "the error omits {name:?}: {err}");
        }
        assert!(err.contains("mostly"), "{err}");
    }

    #[test]
    fn merge_divergence_flags_an_acquire_from_a_copy_the_releaser_never_left() {
        let tmp = with_log(&[
            // Compliant hold: agent-a takes it at aaa, leaves it as bbb.
            &ev_hash(
                "2026-08-11T09:00:00Z",
                "agent-a",
                "acquired",
                "src/printer.rs",
                "aaa",
            ),
            &ev_hash(
                "2026-08-11T09:10:00Z",
                "agent-a",
                "released",
                "src/printer.rs",
                "bbb",
            ),
            // agent-b acquires on a branch that never contained agent-a's change.
            &ev_hash(
                "2026-08-11T09:11:00Z",
                "agent-b",
                "acquired",
                "src/printer.rs",
                "aaa",
            ),
            &ev_hash(
                "2026-08-11T09:20:00Z",
                "agent-b",
                "released",
                "src/printer.rs",
                "ccc",
            ),
            // And a path whose successor DID start from what was left: silent.
            &ev_hash(
                "2026-08-11T09:00:00Z",
                "agent-c",
                "acquired",
                "src/ok.rs",
                "111",
            ),
            &ev_hash(
                "2026-08-11T09:05:00Z",
                "agent-c",
                "released",
                "src/ok.rs",
                "222",
            ),
            &ev_hash(
                "2026-08-11T09:06:00Z",
                "agent-d",
                "acquired",
                "src/ok.rs",
                "222",
            ),
        ]);
        let r = run_check(tmp.path(), Check::MergeDivergence, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.merge_divergences);
        let d = &r.merge_divergences[0];
        assert_eq!(d.path, "src/printer.rs");
        assert_eq!(
            (d.released_by.as_str(), d.released_hash.as_str()),
            ("agent-a", "bbb")
        );
        assert_eq!(
            (d.acquired_by.as_str(), d.acquired_hash.as_str()),
            ("agent-b", "aaa")
        );
        // The renderer must name both sides and say what to look for, or the
        // finding is unactionable.
        let text = render_check(&r);
        assert!(
            text.contains("MERGE DIVERGENCE on src/printer.rs"),
            "{text}"
        );
        assert!(text.contains("NO conflict"), "{text}");
    }

    /// The claim is narrow on purpose: only ADJACENT close/open pairs. A hash that
    /// differs two holds later says nothing, because the intervening holder was
    /// entitled to change the file — and a close anchors exactly one comparison, so
    /// repeated acquires of an unchanged path do not each report it.
    #[test]
    fn merge_divergence_compares_only_adjacent_holds() {
        let tmp = with_log(&[
            &ev_hash("2026-08-11T09:00:00Z", "a", "acquired", "p.rs", "h1"),
            &ev_hash("2026-08-11T09:01:00Z", "a", "released", "p.rs", "h2"),
            // Started from h2: fine. Legitimately changed it to h3.
            &ev_hash("2026-08-11T09:02:00Z", "b", "acquired", "p.rs", "h2"),
            &ev_hash("2026-08-11T09:03:00Z", "b", "released", "p.rs", "h3"),
            // Started from h3: fine, even though it differs from h1 and h2.
            &ev_hash("2026-08-11T09:04:00Z", "c", "acquired", "p.rs", "h3"),
        ]);
        let r = run_check(tmp.path(), Check::MergeDivergence, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.merge_divergences);
    }

    /// Every log written before pact stamped releases has no hash to compare
    /// against. That must be reported as SCOPE, never as a finding — flagging it
    /// would fail every existing repository the moment the check shipped, which is
    /// the same discipline `chain_untracked` and `topology_unstamped` follow.
    ///
    /// And an unhashed close must not leave a stale anchor behind: comparing the
    /// next acquire against a release two holds back would invent a finding.
    #[test]
    fn merge_divergence_treats_an_unhashed_close_as_scope_not_a_finding() {
        let tmp = with_log(&[
            &ev_hash("2026-08-11T09:00:00Z", "a", "acquired", "p.rs", "h1"),
            // A pre-stamping release.
            &ev("2026-08-11T09:01:00Z", "a", "released", "p.rs"),
            // Would have "diverged" from h1 if the walk fell back to the acquire.
            &ev_hash("2026-08-11T09:02:00Z", "b", "acquired", "p.rs", "h9"),
        ]);
        let r = run_check(tmp.path(), Check::MergeDivergence, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.merge_divergences);
        assert_eq!(r.divergence_unhashed, 1);
        assert!(
            render_check(&r).contains("1 close(s) recorded no content hash"),
            "the scope must be stated even when the check is clean: {}",
            render_check(&r)
        );
        // And the clean message must be this check's own, not the one the
        // catch-all arm used to hand every new check.
        assert!(
            render_check(&r).contains("no edit was made against a stale copy"),
            "{}",
            render_check(&r)
        );
    }

    /// pact-1gv.3, the crucible shape reproduced: agent-02 refused src/eval.rs 33
    /// times at 15-second intervals against a median 355s of holder lease left.
    #[test]
    fn retry_storm_names_the_agent_that_hammered_a_held_lease() {
        let mut lines: Vec<String> = vec![ev(
            "2026-08-11T08:00:00Z",
            "agent-06",
            "acquired",
            "src/eval.rs",
        )];
        // 33 refusals, 15 seconds apart, holder always with minutes to spare.
        for i in 0..33 {
            lines.push(ev_refused(
                &format!("2026-08-11T08:{:02}:{:02}Z", (i * 15) / 60, (i * 15) % 60),
                "agent-02",
                "src/eval.rs",
                "agent-06",
                355,
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tmp = with_log(&refs);

        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.retry_storms);
        let st = &r.retry_storms[0];
        assert_eq!(
            (st.agent.as_str(), st.path.as_str()),
            ("agent-02", "src/eval.rs")
        );
        assert_eq!(st.refusals, 33);
        assert_eq!(st.median_gap_secs, Some(15));
        assert_eq!(st.median_holder_remaining_secs, Some(355));
        assert!(!st.ever_claimed, "it never got the path: {st:?}");
        assert_eq!(r.refusals_without_remaining, 0);

        let text = render_check(&r);
        assert!(
            text.contains("RETRY STORM: agent-02 refused src/eval.rs 33 time(s)"),
            "{text}"
        );
        assert!(text.contains("NEVER got the path"), "{text}");
        // The verdict must say the fleet wasted work, not that pact was violated —
        // asking again is allowed.
        assert!(text.contains("Nothing here broke a rule"), "{text}");
    }

    /// Contention that RESOLVED must not be reported. An agent refused once or twice
    /// and then granted the lease is the protocol working, and flagging it is how a
    /// check trains people to ignore it.
    #[test]
    fn retry_storm_ignores_a_refusal_that_resolved_at_a_sane_interval() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "p.rs"),
            // Two polite retries: each waits most of the advertised remaining.
            &ev_refused("2026-08-11T08:00:10Z", "waiter", "p.rs", "holder", 120),
            &ev_refused("2026-08-11T08:02:10Z", "waiter", "p.rs", "holder", 10),
            &ev("2026-08-11T08:02:30Z", "holder", "released", "p.rs"),
            &ev("2026-08-11T08:02:31Z", "waiter", "acquired", "p.rs"),
        ]);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.retry_storms);
        assert!(
            render_check(&r).contains("no agent hammered a lease"),
            "{}",
            render_check(&r)
        );
    }

    /// The impatience half flags a SHORT storm the volume half would miss — three
    /// retries is not a lot, but three retries at 5s against 600s of remaining hold
    /// is the same mistake.
    #[test]
    fn retry_storm_flags_impatience_even_below_the_volume_threshold() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "p.rs"),
            &ev_refused("2026-08-11T08:00:05Z", "waiter", "p.rs", "holder", 600),
            &ev_refused("2026-08-11T08:00:10Z", "waiter", "p.rs", "holder", 595),
            &ev_refused("2026-08-11T08:00:15Z", "waiter", "p.rs", "holder", 590),
        ]);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.retry_storms);
        assert_eq!(r.retry_storms[0].median_gap_secs, Some(5));
    }

    /// `chaos-ghost` is scripts/chaos.sh planting a stale lease. Its failed acquire
    /// is a rail firing correctly, and counting it would credit the fleet with waste
    /// the fault injector caused deliberately.
    #[test]
    fn retry_storm_never_reports_the_fault_injector_as_a_bad_peer() {
        let mut lines = vec![ev(
            "2026-08-11T08:00:00Z",
            "agent-05",
            "acquired",
            "src/printer.rs",
        )];
        for i in 0..9 {
            lines.push(ev_refused(
                &format!("2026-08-11T08:00:{:02}Z", i * 5),
                "chaos-ghost",
                "src/printer.rs",
                "agent-05",
                600,
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tmp = with_log(&refs);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.retry_storms);
    }

    /// A log written before `holder_remaining_secs` existed can still be judged on
    /// COUNT, and the report must say which half it could run. Silence here would let
    /// a clean result read as a stronger claim than it is.
    #[test]
    fn retry_storm_still_counts_on_a_log_that_predates_the_holder_fields() {
        let mut lines = vec![ev("2026-08-11T08:00:00Z", "holder", "acquired", "p.rs")];
        for i in 0..7 {
            lines.push(ev(
                &format!("2026-08-11T08:00:{:02}Z", i * 5),
                "waiter",
                "refused",
                "p.rs",
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tmp = with_log(&refs);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(
            r.findings(),
            1,
            "volume alone must still flag: {:?}",
            r.retry_storms
        );
        assert_eq!(r.retry_storms[0].median_holder_remaining_secs, None);
        assert_eq!(r.refusals_without_remaining, 7);
        assert!(
            render_check(&r).contains("7 refusal(s) carry no holder-remaining"),
            "{}",
            render_check(&r)
        );
    }

    /// pact-as5.6: the assignee is reconstructed by replaying `field=assignee` rows
    /// from the committed export, with no `bd` subprocess anywhere. Three things at
    /// once, because they are one replay: the last row wins, a `field=status` row is
    /// ignored, and a final empty `new_value` means unassigned (so no divergence).
    #[test]
    fn claim_divergence_reads_the_assignee_out_of_interactions_jsonl() {
        let tmp = with_log(&[
            &ev_note(
                "2026-08-11T09:00:00Z",
                "agent-b",
                "src/printer.rs",
                "pact-abc: rewriting the printer",
            ),
            &ev_note(
                "2026-08-11T09:05:00Z",
                "agent-b",
                "src/token.rs",
                "pact-xyz: nobody owns this one",
            ),
        ]);
        with_interactions(
            &tmp,
            &[
                // Ignored: not an assignee change.
                r#"{"id":"int-0","kind":"field_change","created_at":"2026-08-11T08:00:00Z","actor":"someone","issue_id":"pact-abc","extra":{"field":"status","new_value":"in_progress","old_value":"open"}}"#,
                // Reassigned: the LAST row wins, so agent-a owns pact-abc.
                &assignee_row("2026-08-11T08:01:00Z", "pact-abc", "agent-b"),
                &assignee_row("2026-08-11T08:02:00Z", "pact-abc", "agent-a"),
                // Unassigned at the end: resolves to nothing, so no divergence.
                &assignee_row("2026-08-11T08:03:00Z", "pact-xyz", "agent-a"),
                &assignee_row("2026-08-11T08:04:00Z", "pact-xyz", ""),
            ],
        );
        let r = run_check(tmp.path(), Check::ClaimLeaseDivergence, None, false).unwrap();
        assert_eq!(r.claim_unavailable, None);
        assert_eq!(r.findings(), 1, "{:?}", r.claim_divergences);
        let c = &r.claim_divergences[0];
        assert_eq!(c.path, "src/printer.rs");
        assert_eq!(c.agent, "agent-b");
        assert_eq!(c.bead, "pact-abc");
        assert_eq!(c.assignee, "agent-a");
        assert!(
            render_check(&r).contains("CLAIM/LEASE DIVERGENCE on src/printer.rs"),
            "{}",
            render_check(&r)
        );
    }

    /// One malformed line is skipped, not fatal — the rest of the export still
    /// answers. An export is appended to live and can be cut mid-write, the same
    /// hazard `.pact/events.jsonl` has.
    #[test]
    fn a_malformed_interactions_line_is_skipped_and_the_rest_still_resolves() {
        let tmp = with_log(&[&ev_note(
            "2026-08-11T09:00:00Z",
            "agent-b",
            "src/printer.rs",
            "pact-abc: rewriting the printer",
        )]);
        with_interactions(
            &tmp,
            &[
                "{not json at all",
                &assignee_row("2026-08-11T08:02:00Z", "pact-abc", "agent-a"),
                r#"{"kind":"field_change","created_at":"2026-08-11T08:03:00Z""#, // truncated
            ],
        );
        let r = run_check(tmp.path(), Check::ClaimLeaseDivergence, None, false).unwrap();
        assert_eq!(r.claim_unavailable, None);
        assert_eq!(r.findings(), 1, "{:?}", r.claim_divergences);
        assert_eq!(r.claim_divergences[0].assignee, "agent-a");
    }

    /// A wholly unparseable export is "no beads data" and PASSES. Never an error,
    /// never a finding — the same contract `git_unavailable` has, because a check
    /// that cannot run must not read as a clean one.
    #[test]
    fn a_wholly_malformed_interactions_file_reports_no_beads_data_and_passes() {
        let tmp = with_log(&[&ev_note(
            "2026-08-11T09:00:00Z",
            "agent-b",
            "src/printer.rs",
            "pact-abc: rewriting the printer",
        )]);
        with_interactions(&tmp, &["garbage", "", "still not json"]);
        let r = run_check(tmp.path(), Check::ClaimLeaseDivergence, None, false).unwrap();
        assert!(r.claim_divergences.is_empty());
        assert_eq!(r.findings(), 0);
        let reason = r.claim_unavailable.as_deref().unwrap_or_default();
        assert!(reason.contains(".beads/interactions.jsonl"), "{reason}");
        assert!(
            render_check(&r).contains("no beads data"),
            "{}",
            render_check(&r)
        );
    }

    /// No `.beads/` at all — the common case for a repo that never adopted bd, and
    /// the one that proves the check needs no backend.
    #[test]
    fn an_absent_interactions_file_reports_no_beads_data_and_passes() {
        let tmp = with_log(&[&ev_note(
            "2026-08-11T09:00:00Z",
            "agent-b",
            "src/printer.rs",
            "pact-abc: rewriting the printer",
        )]);
        let r = run_check(tmp.path(), Check::ClaimLeaseDivergence, None, false).unwrap();
        assert!(r.claim_divergences.is_empty());
        assert_eq!(r.findings(), 0);
        assert!(r.claim_unavailable.is_some());
    }

    /// pact-7kv: somebody wanted a path, was told no, and learned nothing when it
    /// came free.
    ///
    /// The threshold objection that deferred this bead dissolves once the boundary is
    /// the HOLD: no cutoff to tune, because the holder's own release is the deadline.
    #[test]
    fn silent_contention_flags_a_holder_that_released_without_a_word() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/token.rs"),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/token.rs",
                "holder",
                540,
            ),
            &ev("2026-08-11T08:05:00Z", "holder", "released", "src/token.rs"),
        ]);
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.silent_contentions);
        let sc = &r.silent_contentions[0];
        assert_eq!(sc.path, "src/token.rs");
        assert_eq!(sc.refused_agent, "waiter");
        assert_eq!(sc.holder, "holder");
        assert!(!sc.refused_agent_had_a_channel);

        let text = render_check(&r);
        assert!(text.contains("SILENT CONTENTION on src/token.rs"), "{text}");
        // The remedy must be the channel that works, not a plea to send messages.
        assert!(text.contains("pact watch add"), "{text}");
    }

    /// A watch delivery about the path, inside the hold, IS communication — that is
    /// spec item 4 of pact-8qu, which asked that notified deliveries count.
    #[test]
    fn a_watch_delivery_inside_the_hold_counts_as_communication() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/token.rs"),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/token.rs",
                "holder",
                540,
            ),
            &ev("2026-08-11T08:04:00Z", "holder", "notified", "src/token.rs"),
            &ev("2026-08-11T08:05:00Z", "holder", "released", "src/token.rs"),
        ]);
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.silent_contentions);
    }

    /// pact-1gv.7: the refused agent's OWN subscription counts as using the channel —
    /// and is counted separately rather than netted out, because the same agents then
    /// polled anyway. Crediting the subscription while it busy-retries would score the
    /// run as communicating well at the moment it wasted the most work.
    #[test]
    fn a_refused_agents_own_subscription_counts_and_is_reported_separately() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/ast.rs"),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/ast.rs",
                "holder",
                540,
            ),
            &ev("2026-08-11T08:05:00Z", "holder", "released", "src/ast.rs"),
        ]);
        with_watches(
            &tmp,
            &[&watch_rec(
                "2026-08-11T07:59:00Z",
                "waiter",
                "watch",
                "src/ast.rs",
            )],
        );
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "the channel was in place: {:?}",
            r.silent_contentions
        );
        assert_eq!(r.refusals_with_a_channel, 1);
        // Stated even when clean — a run where every refused agent was subscribed has
        // no findings, and THAT is the interesting fact.
        assert!(
            render_check(&r).contains("1 refusal(s) came from an agent already subscribed"),
            "{}",
            render_check(&r)
        );
    }

    /// Point-in-time, not the live registry. A subscription added AFTER the refusal
    /// must not retroactively excuse it, and one retired before the refusal must not
    /// count either — otherwise a later `watch rm` rewrites history.
    #[test]
    fn a_subscription_only_counts_if_it_was_in_force_at_the_refusal() {
        let base = [
            ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/ast.rs"),
            ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/ast.rs",
                "holder",
                540,
            ),
            ev("2026-08-11T08:05:00Z", "holder", "released", "src/ast.rs"),
        ];
        let refs: Vec<&str> = base.iter().map(String::as_str).collect();

        // Subscribed only AFTER the refusal: does not count.
        let late = with_log(&refs);
        with_watches(
            &late,
            &[&watch_rec(
                "2026-08-11T08:02:00Z",
                "waiter",
                "watch",
                "src/ast.rs",
            )],
        );
        let r = run_check(late.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(
            r.refusals_with_a_channel, 0,
            "a later subscription is not a channel then"
        );
        assert_eq!(r.findings(), 1);

        // Subscribed, then unsubscribed BEFORE the refusal: does not count.
        let gone = with_log(&refs);
        with_watches(
            &gone,
            &[
                &watch_rec("2026-08-11T07:50:00Z", "waiter", "watch", "src/ast.rs"),
                &watch_rec("2026-08-11T07:55:00Z", "waiter", "unwatch", "src/ast.rs"),
            ],
        );
        let r = run_check(gone.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(
            r.refusals_with_a_channel, 0,
            "a retired subscription is not a channel"
        );
        assert_eq!(r.findings(), 1);

        // And a re-add after the unwatch, still before the refusal, counts again.
        let back = with_log(&refs);
        with_watches(
            &back,
            &[
                &watch_rec("2026-08-11T07:50:00Z", "waiter", "watch", "src/ast.rs"),
                &watch_rec("2026-08-11T07:55:00Z", "waiter", "unwatch", "src/ast.rs"),
                &watch_rec("2026-08-11T07:58:00Z", "waiter", "watch", "src/ast.rs"),
            ],
        );
        let r = run_check(back.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.refusals_with_a_channel, 1);
        assert_eq!(r.findings(), 0);
    }

    /// An OPEN hold has not had its chance to communicate yet. Reporting it would
    /// flag a fleet mid-run for something it may be about to do.
    #[test]
    fn silent_contention_ignores_a_hold_the_log_never_shows_closing() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/ast.rs"),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/ast.rs",
                "holder",
                540,
            ),
        ]);
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.silent_contentions);
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

    /// The crucible regression (pact-mqw.1). A holder is SIGKILLed, so it never
    /// releases; a successor steals the path before the TTL lapses, so no
    /// `expired` fires for it either. The `displaced` row is the ONLY thing that
    /// ever closes that window.
    ///
    /// Without it, the dead agent stayed open for the rest of the log and every
    /// later acquire of the path was reported against it. On the real crucible
    /// log that was nine findings, eight naming one killed agent, and not one of
    /// them a concurrent hold. The steal itself is still reported — see the test
    /// below, that part is deliberate — but it must not leak past the steal.
    #[test]
    fn a_killed_holders_window_does_not_leak_past_the_steal() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "victim", "acquired", "src/a.rs"),
            // no release, no expiry: the process was killed
            &ev("2026-08-01T10:05:00Z", "heir", "stolen", "src/a.rs"),
            &ev("2026-08-01T10:05:01Z", "victim", "displaced", "src/a.rs"),
            &ev("2026-08-01T10:09:00Z", "heir", "released", "src/a.rs"),
            // Three later, entirely uncontended acquires of the same path.
            &ev("2026-08-01T10:20:00Z", "agent-c", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:21:00Z", "agent-c", "released", "src/a.rs"),
            &ev("2026-08-01T10:30:00Z", "agent-d", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:31:00Z", "agent-d", "released", "src/a.rs"),
            &ev("2026-08-01T10:40:00Z", "agent-e", "acquired", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(
            r.findings(),
            1,
            "only the steal itself, never the clean acquires after it: {:?}",
            r.double_wins
        );
        assert_eq!(r.double_wins[0].incoming_agent, "heir");
    }

    /// `displaced` closes the victim's window even when the log never shows the
    /// steal opening one for the thief — a truncated or bounded log that starts
    /// mid-takeover must not leave the victim open forever either.
    #[test]
    fn a_displaced_row_closes_its_own_holder_not_the_event_author() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "victim", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:01Z", "victim", "displaced", "src/a.rs"),
            &ev("2026-08-01T10:20:00Z", "later", "acquired", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.double_wins);
    }

    /// A `--steal` over a LIVE lease is a real overlap at the instant it happens
    /// and must be reported. Deliberate, and the reason the `displaced` row that
    /// closes the victim's window is logged AFTER the `stolen` row rather than
    /// before: closing first would silently retire this detection.
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

    /// pact-8qu: every watch kind must be neutral in `reconstruct`, the same
    /// shape `refused`/`renewed`/`restored` already are — a fleet that
    /// subscribes heavily must not have its hold-duration or double-win math
    /// moved by subscriptions or deliveries.
    #[test]
    fn watch_kinds_neither_open_nor_close_a_hold_window() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "watcher", "watched", "src/a.rs"),
            &ev("2026-08-01T10:00:10Z", "holder", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:20Z", "holder", "notified", "src/a.rs"),
            &ev(
                "2026-08-01T10:00:30Z",
                "holder",
                "watch-delivery-failed",
                "src/a.rs",
            ),
            &ev("2026-08-01T10:00:40Z", "holder", "released", "src/a.rs"),
            &ev("2026-08-01T10:00:50Z", "watcher", "unwatched", "src/a.rs"),
        ]);
        let s = summary(tmp.path(), None, false).unwrap();
        // One window, 30s, unmoved by the four watch rows around it.
        let h = s.hold_secs.as_ref().expect("one completed hold");
        assert_eq!(h.completed, 1);
        assert_eq!(h.max_secs, 30);
        assert_eq!(
            run_check(tmp.path(), Check::DoubleWin, None, false)
                .unwrap()
                .findings(),
            0,
            "a subscription must never read as a second holder"
        );
        assert_eq!(s.diffs_delivered, 1);
        assert_eq!(s.deliveries_failed, 1);
        assert!(
            render_summary(&s).contains("diff(s) delivered"),
            "{}",
            render_summary(&s)
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

    // ------------------------------------------------------------ topology

    #[test]
    fn topology_expectations_are_all_or_nothing() {
        let tmp = with_log(&[
            &ev_from("2026-08-01T10:00:00Z", "a", "acquired", "a.rs", "main"),
            &ev_from("2026-08-01T10:01:00Z", "b", "acquired", "b.rs", "wt-b"),
        ]);
        // A mixed run satisfies neither expectation — deliberately, because
        // any "mostly" rule needs a cutoff nobody derived from data.
        let worktrees = run_check(
            tmp.path(),
            Check::Topology(Expect::Worktrees {
                allow_main: Vec::new(),
            }),
            None,
            false,
        )
        .unwrap();
        assert_eq!(worktrees.findings(), 1);
        assert_eq!(worktrees.topology_mismatches[0].invoked_from, "main");

        let main = run_check(tmp.path(), Check::Topology(Expect::Main), None, false).unwrap();
        assert_eq!(main.findings(), 1);
        assert_eq!(main.topology_mismatches[0].invoked_from, "wt-b");

        // `any` is the "just show me" mode and can never fail.
        let any = run_check(tmp.path(), Check::Topology(Expect::Any), None, false).unwrap();
        assert_eq!(any.findings(), 0);
        assert!(
            render_check(&any).contains("expected any"),
            "{}",
            render_check(&any)
        );
    }

    /// `outside` is not a worktree: it means pact ran somewhere that is not
    /// under this repository at all, which is precisely the value that says
    /// the lease/edit binding cannot be assumed.
    #[test]
    fn outside_never_satisfies_expect_worktrees() {
        let tmp = with_log(&[&ev_from(
            "2026-08-01T10:00:00Z",
            "a",
            "acquired",
            "a.rs",
            "outside",
        )]);
        let r = run_check(
            tmp.path(),
            Check::Topology(Expect::Worktrees {
                allow_main: Vec::new(),
            }),
            None,
            false,
        )
        .unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.topology_mismatches);
    }

    /// The convention every existing repository depends on: a log written
    /// before pact recorded invocation context reports "no data" and exits
    /// clean, whatever was expected. Flagging it would have failed every repo
    /// on the day this shipped.
    #[test]
    fn a_pre_stamping_log_never_fails_a_topology_expectation() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        for expect in [
            Expect::Worktrees {
                allow_main: Vec::new(),
            },
            Expect::Main,
            Expect::Any,
        ] {
            let label = format!("{expect:?}");
            let r = run_check(tmp.path(), Check::Topology(expect), None, false).unwrap();
            assert_eq!(
                r.findings(),
                0,
                "{label} must not fail on a pre-stamping log"
            );
            assert_eq!(r.topology_unstamped, 2);
        }
    }

    #[test]
    fn expect_and_check_names_are_validated() {
        assert!(Check::parse("topology", Some("worktrees"), &[]).is_ok());
        assert!(
            Check::parse("topology", None, &[]).is_ok(),
            "bare topology means `any`"
        );
        let bad = Check::parse("topology", Some("mostly"), &[])
            .unwrap_err()
            .to_string();
        for name in Expect::NAMES {
            assert!(bad.contains(name), "the error omits {name:?}: {bad}");
        }
        let unknown = Check::parse("nope", None, &[]).unwrap_err().to_string();
        assert!(
            unknown.contains("topology"),
            "the list must name it: {unknown}"
        );
    }

    /// pact-mqw.10: "covered" was answering the wrong question.
    ///
    /// It asked whether ANYONE held the path across a commit, never whether the
    /// COMMITTER did. In the crucible run one agent was told the protocol did not
    /// apply to it; its worst commit touched five files in one unleased shot, and
    /// every one of those paths was under an active lease held by a compliant peer,
    /// so the commit passed. **The rogue's most damaging commit was invisible
    /// specifically because its peers were compliant.**
    #[test]
    fn a_commit_inside_another_agents_hold_is_its_own_louder_class() {
        let tmp = with_git_log(&[
            // agent-a holds src/ast.rs across the whole window, compliantly.
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/ast.rs"),
            &ev("2026-08-01T10:10:00Z", "agent-a", "released", "src/ast.rs"),
        ]);
        // The rogue commits INSIDE agent-a's hold, and says so in its trailer.
        git_commit_as(
            tmp.path(),
            "src/ast.rs",
            "2026-08-01T10:05:00+00:00",
            "rogue",
        );

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert!(r.git_unavailable.is_none(), "{:?}", r.git_unavailable);
        assert_eq!(
            r.uncovered_commits.len(),
            0,
            "a hold DID span it — that is exactly why the old check passed it"
        );
        assert_eq!(r.cross_held_commits.len(), 1, "{:?}", r.cross_held_commits);
        let x = &r.cross_held_commits[0];
        assert_eq!(x.path, "src/ast.rs");
        assert_eq!(x.committer_agent, "rogue");
        assert_eq!(x.holder, "agent-a");
        assert_eq!(r.findings(), 1, "and it must be a finding, not a note");
        let text = render_check(&r);
        assert!(text.contains("CROSS-HELD COMMIT on src/ast.rs"), "{text}");
        assert!(
            text.contains("1 commit(s) carry a Pact-Agent trailer"),
            "{text}"
        );
        // Louder than uncovered, and the render must say why.
        assert!(text.contains("corrupts theirs"), "{text}");
    }

    /// The holder's OWN commit inside its own hold is the compliant case and must
    /// stay silent — the whole point of the protocol working.
    #[test]
    fn a_commit_inside_your_own_hold_is_not_a_finding() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:10:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit_as(tmp.path(), "a.rs", "2026-08-01T10:05:00+00:00", "agent-a");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{r:?}");
        assert_eq!(r.commits_attributed, 1);
        assert_eq!(r.commits_unattributed, 0);
    }

    /// Degrading, not failing. Every log and every commit that exists today
    /// predates the trailer, so an unattributed commit must behave exactly as it
    /// did before this class existed — and the report must SAY that is the question
    /// it answered, or a clean result reads as a stronger claim than it is.
    #[test]
    fn an_unattributed_commit_keeps_the_old_permissive_behaviour_and_says_so() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:10:00Z", "agent-a", "released", "a.rs"),
        ]);
        // No trailer: this could be the rogue or it could be agent-a, and pact
        // cannot tell. It must not guess from the git author, which every fleet
        // collapses to one identity.
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:05:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.cross_held_commits.len(), 0);
        assert_eq!(r.findings(), 0);
        assert_eq!((r.commits_attributed, r.commits_unattributed), (0, 1));
        let text = render_check(&r);
        assert!(
            text.contains("0 commit(s) carry a Pact-Agent trailer, 1 do not"),
            "{text}"
        );
        assert!(
            text.contains("invisible whenever a compliant peer holds it"),
            "an unattributed run must state the weaker question it answered: {text}"
        );
    }

    /// A mixed history: one attributed cross-held commit, one unattributed commit
    /// in the same hold. Exactly one finding, and both counted.
    #[test]
    fn a_mixed_history_reports_only_what_it_can_attribute() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "m.rs"),
            &ev("2026-08-01T10:30:00Z", "agent-a", "released", "m.rs"),
        ]);
        git_commit(tmp.path(), "m.rs", "2026-08-01T10:05:00+00:00");
        git_commit_as(tmp.path(), "m.rs", "2026-08-01T10:10:00+00:00", "rogue");
        git_commit_as(tmp.path(), "m.rs", "2026-08-01T10:15:00+00:00", "agent-a");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!((r.commits_attributed, r.commits_unattributed), (2, 1));
        assert_eq!(r.cross_held_commits.len(), 1, "{:?}", r.cross_held_commits);
        assert_eq!(r.cross_held_commits[0].committer_agent, "rogue");
    }

    /// FINDING 5b: `--expect worktrees` could not pass for any real fleet, because in the
    /// topology pact documents somebody must sit in the main checkout — it is where the
    /// coordination logs are committed from. Run 5 failed with 19 offending events, not one
    /// of which was an agent working in the wrong place.
    #[test]
    fn allow_main_excuses_a_declared_orchestrator_and_nobody_else() {
        let tmp = with_log(&[
            &ev_from(
                "2026-08-01T10:00:00Z",
                "agent-a",
                "acquired",
                "a.rs",
                "wt-a",
            ),
            &ev_from(
                "2026-08-01T10:01:00Z",
                "orchestrator",
                "acquired",
                "b.rs",
                "main",
            ),
        ]);

        // Without the exception, the orchestrator's own protocol-following lease fails it.
        let bare = Check::parse("topology", Some("worktrees"), &[]).unwrap();
        let r = run_check(tmp.path(), bare, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.topology_mismatches);

        // With it, the run passes and the exception is COUNTED — an exception nobody can
        // see the size of stops being read as one.
        let allowed =
            Check::parse("topology", Some("worktrees"), &["orchestrator".to_string()]).unwrap();
        let r = run_check(tmp.path(), allowed, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.topology_mismatches);
        assert_eq!(r.topology_allowed_from_main, 1);
        assert!(render_check(&r).contains("excused from the main checkout"));
    }

    /// Naming an identity excuses it from `main` ONLY. It is not a licence to act from
    /// anywhere, and it does not excuse anyone else.
    #[test]
    fn allow_main_does_not_excuse_another_agent_or_another_location() {
        let tmp = with_log(&[
            &ev_from("2026-08-01T10:00:00Z", "stray", "acquired", "a.rs", "main"),
            &ev_from(
                "2026-08-01T10:01:00Z",
                "orchestrator",
                "acquired",
                "b.rs",
                "outside",
            ),
        ]);
        let check =
            Check::parse("topology", Some("worktrees"), &["orchestrator".to_string()]).unwrap();
        let r = run_check(tmp.path(), check, None, false).unwrap();
        assert_eq!(
            r.findings(),
            2,
            "an unlisted agent from main and a listed one from outside both fail: {:?}",
            r.topology_mismatches
        );
        assert_eq!(r.topology_allowed_from_main, 0);
    }

    /// pact-b73.6, the exact answer: with a head on both boundaries the hold brackets a
    /// real commit range, so the correlation is a lookup rather than an inference from
    /// wall-clock time.
    ///
    /// The timestamps here are deliberately WRONG — the commit's clock puts it far
    /// outside the hold's window — so a pass can only come from the range being used.
    /// That is the whole point: on a busy fleet the window is what misattributes.
    #[test]
    fn a_hold_is_correlated_by_its_recorded_head_range_not_by_time() {
        let tmp = with_git_log(&[]);
        git_commit(tmp.path(), "seed.rs", "2026-08-01T09:00:00+00:00");
        let before = head_of(tmp.path());
        git_commit(tmp.path(), "a.rs", "2026-08-01T23:59:00+00:00");
        let after = head_of(tmp.path());

        let log = tmp.path().join(".pact/events.jsonl");
        std::fs::write(
            &log,
            format!(
                "{}\n{}\n",
                ev_head(
                    "2026-08-01T10:00:00Z",
                    "agent-a",
                    "acquired",
                    "a.rs",
                    &before
                ),
                ev_head(
                    "2026-08-01T10:02:00Z",
                    "agent-a",
                    "released",
                    "a.rs",
                    &after
                ),
            ),
        )
        .unwrap();

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.correlated_by_head, 1, "the range must be used");
        assert_eq!(r.correlated_by_time, 0);
        assert!(
            r.holds_with_no_commit.is_empty(),
            "the commit is inside the RANGE even though it is outside the window: {:?}",
            r.holds_with_no_commit
        );
    }

    /// And the range must be able to say NO: a hold whose range contains commits, none
    /// of them touching the held path, still reports the hold as uncommitted.
    #[test]
    fn a_head_range_that_touches_other_paths_does_not_credit_the_held_one() {
        let tmp = with_git_log(&[]);
        git_commit(tmp.path(), "seed.rs", "2026-08-01T09:00:00+00:00");
        let before = head_of(tmp.path());
        git_commit(tmp.path(), "elsewhere.rs", "2026-08-01T10:01:00+00:00");
        let after = head_of(tmp.path());

        std::fs::write(
            tmp.path().join(".pact/events.jsonl"),
            format!(
                "{}\n{}\n",
                ev_head(
                    "2026-08-01T10:00:00Z",
                    "agent-a",
                    "acquired",
                    "a.rs",
                    &before
                ),
                ev_head(
                    "2026-08-01T10:02:00Z",
                    "agent-a",
                    "released",
                    "a.rs",
                    &after
                ),
            ),
        )
        .unwrap();

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.correlated_by_head, 1);
        assert_eq!(r.holds_with_no_commit.len(), 1, "a.rs was never committed");
    }

    /// The fallback that is not optional: every log written before pact stamped `head`
    /// has none, and the check must degrade to the timestamp window and SAY it did
    /// rather than silently reporting fewer findings.
    #[test]
    fn a_hold_with_no_recorded_head_falls_back_to_the_timestamp_window_and_says_so() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.correlated_by_head, 0);
        assert_eq!(r.correlated_by_time, 1);
        assert!(r.holds_with_no_commit.is_empty(), "the window still works");
        let text = render_check(&r);
        assert!(
            text.contains("TIMESTAMP WINDOW"),
            "the reader must be told which path was taken:\n{text}"
        );
    }

    /// A recorded hash that no longer resolves — a deleted and gc'd worktree branch, a
    /// force-push, a shallow clone — must degrade to the window rather than report
    /// nothing. Reporting nothing would read as "this hold landed no commit", which is
    /// a false finding rather than a missing one.
    #[test]
    fn a_head_that_no_longer_resolves_degrades_to_the_window_rather_than_reporting_nothing() {
        let tmp = with_git_log(&[
            &ev_head(
                "2026-08-01T10:00:00Z",
                "agent-a",
                "acquired",
                "a.rs",
                "dead1ee",
            ),
            &ev_head(
                "2026-08-01T10:02:00Z",
                "agent-a",
                "released",
                "a.rs",
                "dead2ee",
            ),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(
            r.correlated_by_head, 0,
            "an unresolvable range is not a usable one"
        );
        assert_eq!(r.correlated_by_time, 1);
        assert!(
            r.holds_with_no_commit.is_empty(),
            "the fallback found the commit the range could not: {:?}",
            r.holds_with_no_commit
        );
    }

    /// Both eras in one log, which is what an upgrading repository actually looks like.
    #[test]
    fn a_mixed_log_correlates_each_hold_by_whatever_it_recorded() {
        let tmp = with_git_log(&[]);
        git_commit(tmp.path(), "seed.rs", "2026-08-01T09:00:00+00:00");
        let before = head_of(tmp.path());
        git_commit(tmp.path(), "new.rs", "2026-08-01T10:01:00+00:00");
        let after = head_of(tmp.path());
        git_commit(tmp.path(), "old.rs", "2026-08-01T11:01:00+00:00");

        std::fs::write(
            tmp.path().join(".pact/events.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n",
                ev_head(
                    "2026-08-01T10:00:00Z",
                    "agent-a",
                    "acquired",
                    "new.rs",
                    &before
                ),
                ev_head(
                    "2026-08-01T10:02:00Z",
                    "agent-a",
                    "released",
                    "new.rs",
                    &after
                ),
                ev("2026-08-01T11:00:00Z", "agent-b", "acquired", "old.rs"),
                ev("2026-08-01T11:02:00Z", "agent-b", "released", "old.rs"),
            ),
        )
        .unwrap();

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!((r.correlated_by_head, r.correlated_by_time), (1, 1));
        assert!(
            r.holds_with_no_commit.is_empty(),
            "each hold found its commit by its own route: {:?}",
            r.holds_with_no_commit
        );
        let text = render_check(&r);
        assert!(
            text.contains("1 hold(s) correlated by their recorded HEAD range"),
            "{text}"
        );
    }

    #[test]
    fn a_hold_with_a_commit_inside_its_window_is_not_reported() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert!(r.git_unavailable.is_none(), "{:?}", r.git_unavailable);
        assert_eq!(r.findings(), 0);
        assert!(
            r.holds_with_no_commit.is_empty(),
            "a commit landed inside the window: {:?}",
            r.holds_with_no_commit
        );
    }

    /// Informational only — read-only work (research, review, a lease taken
    /// and then released with nothing to show for it) is a legitimate
    /// outcome, not a defect, so this must never move the exit code.
    #[test]
    fn a_hold_with_no_commit_at_all_is_reported_but_is_not_a_finding() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        // Not one line of history exists for a.rs at all — the "read-only
        // lease" shape, distinct from a commit landing outside the window
        // (that is `uncovered_commits`'s job, covered separately below).
        git_commit(tmp.path(), "unrelated.rs", "2026-08-01T10:01:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "an uncommitted hold must not fail the check"
        );
        assert_eq!(r.holds_with_no_commit.len(), 1);
        assert_eq!(r.holds_with_no_commit[0].path, "a.rs");
        assert_eq!(r.holds_with_no_commit[0].agent, "agent-a");
        let text = render_check(&r);
        assert!(text.contains("informational"), "{text}");
        assert!(text.contains("a.rs"), "{text}");
    }

    /// The finding this check exists for: not just overlapping lease
    /// events (already `--check double-win`'s job) but real commits landing
    /// from both sides during the overlap.
    #[test]
    fn two_holds_with_commits_landing_in_their_overlap_are_flagged() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            // agent-b's acquire overlaps agent-a's still-open hold.
            &ev("2026-08-01T10:01:00Z", "agent-b", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
            &ev("2026-08-01T10:03:00Z", "agent-b", "released", "a.rs"),
        ]);
        // Both commits fall inside the overlap window [10:01, 10:02].
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:10+00:00");
        // A second, distinct commit needs a real change to land in history —
        // git commits a no-op tree change once, so touch a second file to
        // force a second commit at the timestamp that matters.
        std::fs::write(tmp.path().join(".marker"), "2").unwrap();
        let run = |args: &[&str], at: &str| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "tester")
                .env("GIT_AUTHOR_EMAIL", "tester@example.com")
                .env("GIT_AUTHOR_DATE", at)
                .env("GIT_COMMITTER_NAME", "tester")
                .env("GIT_COMMITTER_EMAIL", "tester@example.com")
                .env("GIT_COMMITTER_DATE", at)
                .status()
                .unwrap()
        };
        let at = "2026-08-01T10:01:20+00:00";
        std::fs::write(tmp.path().join("a.rs"), "second write").unwrap();
        assert!(run(&["add", "a.rs"], at).success());
        assert!(run(&["commit", "--quiet", "-m", "second touch"], at).success());

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.findings(), 1, "the concurrent write is a real finding");
        assert_eq!(r.concurrent_writes.len(), 1);
        let w = &r.concurrent_writes[0];
        assert_eq!(w.path, "a.rs");
        assert_eq!(w.commits_in_overlap.len(), 2);
        assert!([&w.first_agent, &w.second_agent].contains(&&"agent-a".to_string()));
        assert!([&w.first_agent, &w.second_agent].contains(&&"agent-b".to_string()));
        let text = render_check(&r);
        assert!(text.contains("CONCURRENT WRITE"), "{text}");
        assert!(text.contains("double-win"), "{text}");
    }

    /// The commit-correlation counterpart to "did anyone hold this path" —
    /// did anyone hold it AT THE TIME this commit landed. A commit outside
    /// every recorded window for a path pact does otherwise coordinate is
    /// work done with no lease at all.
    #[test]
    fn a_commit_outside_every_hold_for_a_leased_path_is_uncovered() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        // Outside the [10:00, 10:02] window, with nothing else leasing it.
        git_commit(tmp.path(), "a.rs", "2026-08-01T12:00:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        assert_eq!(r.uncovered_commits.len(), 1);
        assert_eq!(r.uncovered_commits[0].path, "a.rs");
        let text = render_check(&r);
        assert!(text.contains("UNCOVERED COMMIT"), "{text}");
    }

    /// A path nobody ever leased is a different question this check does not
    /// try to answer — most of a real repository is never leased at any
    /// given moment, and flagging all of it would be pure noise.
    #[test]
    fn a_commit_to_a_never_leased_path_is_never_flagged() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:00+00:00");
        // README.md is never leased by anyone in this log.
        git_commit(tmp.path(), "README.md", "2026-08-01T13:00:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.findings(), 0);
        assert!(r.uncovered_commits.is_empty());
    }

    /// `with_log`'s `.git` is a bare directory, not a real repository — the
    /// shape this check must degrade cleanly against rather than panicking
    /// or reporting a false, blanket set of findings.
    #[test]
    fn a_repository_git_cannot_actually_read_degrades_without_crashing() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "no git history to correlate against must never itself be a finding"
        );
    }
}
