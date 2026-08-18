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

mod checks;
mod context;
mod export;
#[cfg(test)]
mod fixtures;
mod model;
mod summary;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::events::ChainMismatch;
use checks::claim_lease_divergence::{claim_divergences, ClaimDivergence};
use checks::commit_correlation::{
    ConcurrentWrite, CrossHeldCommit, UncommittedHold, UncoveredCommit,
};
use checks::merge_divergence::{merge_divergences, MergeDivergence};
use checks::retry_storm::retry_storms;
use checks::silent_contention::{silent_contentions, SilentContention};
use checks::topology::TopologyMismatch;
use context::load;
use model::{reconstruct, DoubleWin, Hold};

pub use checks::retry_storm::RetryStorm;
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

    pub(in crate::audit) fn label(&self) -> &'static str {
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
    pub(in crate::audit) fn allowed_from_main(&self) -> &[String] {
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
    pub(in crate::audit) fn satisfied_by(&self, invoked_from: &str) -> bool {
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
        Check::StaleHolds => checks::stale_holds::detect(holds, &mut report),
        Check::ChainIntegrity => checks::chain_integrity::detect(repo_root, &mut report)?,
        Check::CommitCorrelation => checks::commit_correlation::detect(
            repo_root,
            &loaded.context,
            &events,
            &holds,
            &mut report,
        ),
        Check::MergeDivergence => {
            let (divergences, unhashed) = merge_divergences(&events);
            report.merge_divergences = divergences;
            report.divergence_unhashed = unhashed;
        }
        Check::ClaimLeaseDivergence => claim_divergences(repo_root, &events, &mut report),
        Check::RetryStorm => retry_storms(&events, &mut report),
        Check::SilentContention => silent_contentions(repo_root, &events, &holds, &mut report),
        Check::Topology(ref expect) => {
            checks::topology::detect(&loaded.context, &events, expect, &mut report)
        }
    }
    Ok(report)
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
        checks::topology::scope(r, &mut out);
    }

    if r.check == "chain-integrity" {
        checks::chain_integrity::scope(r, &mut out);
    }

    if r.check == "merge-divergence" {
        checks::merge_divergence::scope(r, &mut out);
    }

    if r.check == "claim-lease-divergence" && checks::claim_lease_divergence::scope(r, &mut out) {
        return out.join("\n");
    }

    if r.check == "silent-contention" {
        checks::silent_contention::scope(r, &mut out);
    }

    if r.check == "retry-storm" {
        checks::retry_storm::scope(r, &mut out);
    }

    if r.check == "commit-correlation" && checks::commit_correlation::scope(r, &mut out) {
        return out.join("\n");
    }

    if r.findings() == 0 {
        out.push(match r.check {
            "double-win" => {
                "no overlapping hold windows — no two agents ever held one path at once".to_string()
            }
            "chain-integrity" => checks::chain_integrity::clean(),
            "commit-correlation" => checks::commit_correlation::clean(),
            "topology" => checks::topology::clean(r),
            "silent-contention" => checks::silent_contention::clean(),
            "retry-storm" => checks::retry_storm::clean(),
            "claim-lease-divergence" => checks::claim_lease_divergence::clean(),
            "merge-divergence" => checks::merge_divergence::clean(),
            // `stale-holds` named explicitly rather than left to the catch-all it
            // used to own: a new check landing on that arm inherited the wrong
            // clean message, which is how this comment got written.
            _ => checks::stale_holds::clean(),
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

    checks::stale_holds::findings(r, &mut out);

    checks::chain_integrity::findings(r, &mut out);

    checks::commit_correlation::findings(r, &mut out);

    checks::topology::findings(r, &mut out);

    checks::silent_contention::findings(r, &mut out);

    checks::retry_storm::findings(r, &mut out);

    checks::claim_lease_divergence::findings(r, &mut out);

    checks::merge_divergence::findings(r, &mut out);

    checks::commit_correlation::correlation_footer(r, &mut out);

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

    // ------------------------------------------------------------ topology

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
}
