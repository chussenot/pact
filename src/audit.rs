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
//! Audit never opens `.beads/`, a Dolt directory, a SQLite file or a JSONL
//! export, because "pact never touches the Beads store directly, only the
//! CLI" is an invariant the whole messaging design rests on, and an analytics
//! command is exactly where it would be convenient to break it. Beads-side
//! questions live in `scripts/beads-retro.sh`, which is best-effort,
//! jq-based, and says so in its header.
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
    /// The one check that asks the Beads store a question rather than reading only
    /// `.pact/` — see [`ClaimDivergence`] for the caveat that comes with it.
    ClaimLeaseDivergence,
}

/// What `--expect` declares a run's topology should have been.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Every stamped event was invoked from a linked worktree.
    Worktrees,
    /// Every stamped event was invoked from the main checkout.
    Main,
    /// Nothing to fail — report the distribution and exit 0.
    Any,
}

impl Expect {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "worktrees" => Ok(Expect::Worktrees),
            "main" => Ok(Expect::Main),
            "any" => Ok(Expect::Any),
            other => Err(anyhow::anyhow!(
                "unknown --expect \"{other}\"; expected worktrees, main or any"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Expect::Worktrees => "worktrees",
            Expect::Main => "main",
            Expect::Any => "any",
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
    fn satisfied_by(self, invoked_from: &str) -> bool {
        match self {
            Expect::Any => true,
            Expect::Main => invoked_from == "main",
            // "outside" is not a worktree: it means pact ran somewhere that is
            // not under this repository at all, which is the one value that
            // says the lease/edit binding cannot be assumed.
            Expect::Worktrees => invoked_from != "main" && invoked_from != "outside",
        }
    }
}

impl Check {
    /// `expect` is only meaningful for `topology`; every other check ignores
    /// it, and clap requires `--check` alongside it so it cannot be passed
    /// alone.
    pub fn parse(s: &str, expect: Option<&str>) -> Result<Self> {
        match s {
            "double-win" => Ok(Check::DoubleWin),
            "stale-holds" => Ok(Check::StaleHolds),
            "chain-integrity" => Ok(Check::ChainIntegrity),
            "commit-correlation" => Ok(Check::CommitCorrelation),
            "merge-divergence" => Ok(Check::MergeDivergence),
            "claim-lease-divergence" => Ok(Check::ClaimLeaseDivergence),
            "topology" => Ok(Check::Topology(match expect {
                Some(e) => Expect::parse(e)?,
                // Defaulting to `any` rather than erroring: `--check topology`
                // alone is a legitimate "show me the distribution", and the
                // summary says the same thing without a flag.
                None => Expect::Any,
            })),
            other => Err(anyhow::anyhow!(
                "unknown check \"{other}\"; expected double-win, stale-holds, chain-integrity, \
                 commit-correlation, merge-divergence, claim-lease-divergence or topology"
            )),
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
/// assignee *now*, not at acquire time — the log records the note, not the bead's
/// state when it was written. So a hold that legitimately handed its bead on
/// afterwards shows up here too. This answers "whose bead did this hold's note
/// name, and who owns that bead today", which is the retrospective question; the
/// live question is answered at acquire time, where the assignee really is current.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimDivergence {
    pub path: String,
    pub agent: String,
    pub bead: String,
    pub assignee: String,
    pub acquired_at: String,
    pub line: usize,
}

/// Cross-check each hold's note against the Beads store.
///
/// Widens audit's usual "`.pact/` and nothing else" scope for the second time, the
/// same way [`Check::CommitCorrelation`] widened it for git — and under the same
/// rule: the invariant that section protects is "never touch the Beads DB
/// directly, only its CLI", which this obeys. `pact audit --export` already asks
/// bd a question for `unacknowledged_messages`.
///
/// One lookup per DISTINCT bead, not per hold: a wave of twenty holds naming one
/// bead is one subprocess.
fn claim_divergences(
    repo_root: &std::path::Path,
    events: &[(usize, Event)],
    report: &mut CheckReport,
) {
    let cli = match crate::beads::BeadsCli::locate() {
        Ok(cli) => cli,
        Err(e) => {
            report.claim_unavailable = Some(format!("{e:#}"));
            return;
        }
    };
    // bead id -> assignee, resolved once. `None` means asked and unassigned (or
    // unknown), which is not worth asking twice either.
    let mut resolved: BTreeMap<String, Option<String>> = BTreeMap::new();
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
        let assignee = resolved
            .entry(bead.to_string())
            .or_insert_with(|| crate::beads::assignee_of(&cli, repo_root, bead))
            .clone();
        let Some(assignee) = assignee else { continue };
        if assignee == e.agent {
            continue;
        }
        report.claim_divergences.push(ClaimDivergence {
            path: path.to_string(),
            agent: e.agent.clone(),
            bead: bead.to_string(),
            assignee,
            acquired_at: e.at.clone(),
            line: *line,
        });
    }
}

/// One invocation point that contradicted `--expect`.
#[derive(Debug, Clone, Serialize)]
pub struct TopologyMismatch {
    pub invoked_from: String,
    pub events: usize,
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
    /// Takeovers of a lease that had already lapsed — an `expired` row for the
    /// path precedes them. Counted apart from `steals`, which is reserved for a
    /// forced `--steal` over a claim that was still live (pact-mqw.2).
    #[serde(default)]
    pub reclaims: usize,
    pub open_holds: usize,
    pub hold_secs: Option<HoldStats>,
    pub top_contended: Vec<Contended>,
    pub per_agent: Vec<AgentActivity>,
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
    /// `Check::CommitCorrelation` only: `Some(reason)` when `git log` could
    /// not be read at all — missing binary, or an I/O error running it. The
    /// three commit-based fields below are then always empty, which must
    /// read as "this check could not run", never as "nothing found".
    pub git_unavailable: Option<String>,
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
    /// `Check::ClaimLeaseDivergence` only: `Some(reason)` when the Beads store
    /// could not be asked at all. `claim_divergences` is then always empty, which
    /// must read as "this check could not run", never as "nothing found" — the same
    /// contract `git_unavailable` has.
    pub claim_unavailable: Option<String>,
    /// `Check::ClaimLeaseDivergence` only: holds whose note named no bead, so there
    /// was nothing to cross-check. Scope, not a finding.
    pub holds_naming_no_bead: usize,
    /// `Check::Topology` only: events carrying no `invoked_from` at all.
    /// Reported, never a finding — every log written before pact 0.7.0 is
    /// entirely in this state, and flagging it would fail every existing
    /// repository the moment this shipped. Same discipline as
    /// `chain_untracked`.
    pub topology_unstamped: usize,
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
///
/// A forced `--steal` is NOT in this list, and deliberately so: it closes the
/// displaced holder from inside the `opens` arm, after recording the overlap the
/// steal genuinely represents (pact-mqw.1). `displaced` is not here either — it
/// is a feed row, and the `stolen` before it already did the closing.
fn closes(kind: &str) -> bool {
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
    open: &mut BTreeMap<String, (usize, String, usize, u64, bool)>,
    holds: &mut Vec<Hold>,
    path: &str,
    agent: &str,
    line: usize,
    at: &str,
    closed_by: &str,
) -> bool {
    let Some((oline, oat, renewals, ttl, ttl_assumed)) = open.remove(agent) else {
        return false;
    };
    let held = parse_at(&oat)
        .zip(parse_at(at))
        .map(|(a, b)| (b - a).num_seconds());
    holds.push(Hold {
        path: path.to_string(),
        agent: agent.to_string(),
        opened_line: oline,
        opened_at: oat,
        closed_line: Some(line),
        closed_at: Some(at.to_string()),
        closed_by: Some(closed_by.to_string()),
        renewals,
        held_secs: held,
        ttl_secs: ttl,
        ttl_assumed,
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
                        close_window(&mut open, &mut holds, path, &victim, line, &e.at, "stolen");
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
                );
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
        });
        a.events += 1;
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
        events: events.len(),
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
            Check::CommitCorrelation => "commit-correlation",
            Check::Topology(_) => "topology",
            Check::MergeDivergence => "merge-divergence",
            Check::ClaimLeaseDivergence => "claim-lease-divergence",
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
        git_unavailable: None,
        holds_with_no_commit: Vec::new(),
        concurrent_writes: Vec::new(),
        uncovered_commits: Vec::new(),
        cross_held_commits: Vec::new(),
        commits_attributed: 0,
        commits_unattributed: 0,
        topology_mismatches: Vec::new(),
        expected_topology: None,
        topology_unstamped: 0,
        merge_divergences: Vec::new(),
        divergence_unhashed: 0,
        claim_divergences: Vec::new(),
        claim_unavailable: None,
        holds_naming_no_bead: 0,
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
        Check::CommitCorrelation => correlate_commits(repo_root, &events, &holds, &mut report),
        Check::MergeDivergence => {
            let (divergences, unhashed) = merge_divergences(&events);
            report.merge_divergences = divergences;
            report.divergence_unhashed = unhashed;
        }
        Check::ClaimLeaseDivergence => claim_divergences(repo_root, &events, &mut report),
        Check::Topology(expect) => {
            report.expected_topology = Some(expect.label());
            let mut by_point: BTreeMap<String, usize> = BTreeMap::new();
            for (_, e) in &events {
                match e.invoked_from.as_deref() {
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

/// `pact audit --export` (pact-1l8.2): one self-contained snapshot bundling
/// the summary, every named check and `pact doctor`'s checks — the exact set
/// of things the arkanoid field audit (pact-juz) had to assemble by hand,
/// from a separate `pact doctor`, a separate `pact audit` per check, and a
/// raw grep of `.pact/events.jsonl`. Meant to be read directly by a human, or
/// handed to another agent session asking "how is pact actually being used,
/// and where does it fall down" — the same question that produced pact-juz.
#[derive(Debug, Serialize)]
pub struct ExportReport {
    pub summary: Summary,
    pub double_win: CheckReport,
    pub stale_holds: CheckReport,
    pub chain_integrity: CheckReport,
    pub commit_correlation: CheckReport,
    /// pact-mqw.3: successive holds that started from divergent content.
    pub merge_divergence: CheckReport,
    /// pact-mqw.4: holds whose note named a bead assigned to another agent.
    pub claim_lease_divergence: CheckReport,
    /// Run with `--expect any`, so it reports the distribution rather than a
    /// verdict: a stored retrospective should not fail on an expectation
    /// nobody declared when it was written.
    pub topology: CheckReport,
    pub doctor: crate::doctor::DoctorReport,
    /// Messages their own recipient never marked read (pact-ler.3).
    ///
    /// Here rather than in `pact doctor`, and that placement is forced rather
    /// than preferred. Answering this needs a real `bd list`, and `bd` takes a
    /// `.beads/.write.lock` to serve one — while `doctor` is exposed over MCP
    /// as `pact_doctor`, which docs/mcp.md promises is strictly read-only and
    /// `tests/mcp.rs::every_tool_call_leaves_the_repository_byte_identical`
    /// enforces byte-for-byte. A doctor check would have quietly broken that
    /// guarantee for every MCP observer. `--export` is CLI-only (MCP exposes
    /// `audit::summary`, never this), deliberately run at the end of a run,
    /// and is exactly the retrospective artifact this question belongs in.
    ///
    /// Empty when there is no Beads CLI or no `.beads/` — "cannot ask" and
    /// "nothing pending" are both reported as nothing pending here, because
    /// the sibling `doctor` section of this same report already says loudly
    /// when the backend is missing.
    pub unacknowledged_messages: Vec<crate::msg::Message>,
    /// Short, human-readable highlights pulled out of the structured fields
    /// above, so a reader does not have to re-derive "is this worth looking
    /// at" from raw counts and thresholds. Empty means nothing here rose to
    /// that bar — not that nothing was checked; the structured fields are
    /// always the full data regardless of what lands here.
    pub observations: Vec<String>,
}

pub fn export(
    repo_root: &std::path::Path,
    since: Option<DateTime<Utc>>,
    include_annotated: bool,
) -> Result<ExportReport> {
    let summary_report = summary(repo_root, since, include_annotated)?;
    let double_win = run_check(repo_root, Check::DoubleWin, since, include_annotated)?;
    let stale_holds = run_check(repo_root, Check::StaleHolds, since, include_annotated)?;
    let chain_integrity = run_check(repo_root, Check::ChainIntegrity, since, include_annotated)?;
    let topology = run_check(
        repo_root,
        Check::Topology(Expect::Any),
        since,
        include_annotated,
    )?;
    let commit_correlation = run_check(
        repo_root,
        Check::CommitCorrelation,
        since,
        include_annotated,
    )?;
    let merge_divergence = run_check(repo_root, Check::MergeDivergence, since, include_annotated)?;
    let claim_lease_divergence = run_check(
        repo_root,
        Check::ClaimLeaseDivergence,
        since,
        include_annotated,
    )?;
    let doctor = crate::doctor::checks(repo_root);

    // Best-effort by design: a repo with no `.beads/` or no backend on PATH
    // is not a repo with a messaging problem, and `doctor` above already
    // reports a missing CLI on its own line.
    let unacknowledged_messages = if repo_root.join(".beads").exists() {
        crate::beads::BeadsCli::locate()
            .ok()
            .and_then(|cli| crate::msg::unacknowledged(&cli, repo_root).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut observations = Vec::new();
    if double_win.findings() > 0 {
        observations.push(format!(
            "{} double-win(s): two agents held one path at the same time — see pact-ehi.",
            double_win.findings()
        ));
    }
    if stale_holds.findings() > 0 {
        observations.push(format!(
            "{} stale hold(s): ran past their own recorded TTL with no renew.",
            stale_holds.findings()
        ));
    }
    if chain_integrity.findings() > 0 {
        observations.push(format!(
            "{} chain-integrity break(s): a chain-tracked line does not match its recorded hash.",
            chain_integrity.findings()
        ));
    }
    match &commit_correlation.git_unavailable {
        Some(reason) => observations.push(format!("commit-correlation could not run: {reason}")),
        None => {
            if !commit_correlation.concurrent_writes.is_empty() {
                observations.push(format!(
                    "{} concurrent write(s): real commits landed from both sides of an \
                     overlapping hold.",
                    commit_correlation.concurrent_writes.len()
                ));
            }
            if !commit_correlation.uncovered_commits.is_empty() {
                observations.push(format!(
                    "{} commit(s) landed on a leased path with no hold covering them.",
                    commit_correlation.uncovered_commits.len()
                ));
            }
            // `commits_attributed`/`commits_unattributed` are deliberately NOT
            // observations: they are scope, and `observations` is the findings
            // list. A clean history must not manufacture an entry there. The
            // counts ride in the report itself and in `render_check`'s scope line.
            if !commit_correlation.cross_held_commits.is_empty() {
                observations.push(format!(
                    "{} commit(s) landed on a path a DIFFERENT agent held, per their Pact-Agent \
                     trailer.",
                    commit_correlation.cross_held_commits.len()
                ));
            }
        }
    }
    if merge_divergence.findings() > 0 {
        observations.push(format!(
            "{} hold(s) started from a copy the previous holder never produced: an edit made \
             against a stale worktree, which git often merges with no conflict marker.",
            merge_divergence.findings()
        ));
    }
    // No observation for `claim_unavailable`, unlike commit-correlation's
    // `git_unavailable`: `doctor` already reports a missing Beads CLI on its own
    // line in this same list, and the reason a check could not run is scope rather
    // than a finding. `observations` is the findings list — a repo with no backend
    // must not read as a repo with a coordination problem.
    if claim_lease_divergence.findings() > 0 {
        observations.push(format!(
            "{} hold(s) named a bead their holder does not own: the bead claim and the file \
             lease are separate locks that do not consult each other.",
            claim_lease_divergence.findings()
        ));
    }
    if !unacknowledged_messages.is_empty() {
        // Named, not counted, and "nobody read it" kept distinct from "read
        // by somebody who was not the addressee" — the second is the common
        // field shape (`--to-owner-of` means a message about a path follows
        // the path, so the agent who picks that path up is often the reader),
        // and collapsing them would read as "nobody looked".
        observations.push(format!(
            "{} message(s) never read by their recipient: {}. `pact msg sent` shows these as \
             undelivered to whoever sent them.",
            unacknowledged_messages.len(),
            unacknowledged_messages
                .iter()
                .map(|m| {
                    if m.read_by.is_empty() {
                        format!("{} (to {}, nobody has read it)", m.id, m.to)
                    } else {
                        format!(
                            "{} (to {}, read only by {})",
                            m.id,
                            m.to,
                            m.read_by.join("/")
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for c in &doctor.checks {
        if !c.ok || c.warn {
            observations.push(format!(
                "doctor: {} {} — {}",
                if c.ok { "warn" } else { "FAIL" },
                c.name,
                c.detail
            ));
        }
    }

    Ok(ExportReport {
        summary: summary_report,
        double_win,
        stale_holds,
        chain_integrity,
        commit_correlation,
        merge_divergence,
        claim_lease_divergence,
        topology,
        doctor,
        unacknowledged_messages,
        observations,
    })
}

/// One number that moved between two exported reports.
#[derive(Debug, Clone, Serialize)]
pub struct Movement {
    pub field: &'static str,
    pub baseline: i64,
    pub current: i64,
    pub delta: i64,
}

/// What changed between a baseline export and this repository now
/// (pact-okz.2).
///
/// Reports movement and **never a verdict**. "Contention fell from 8 agents on
/// one path to 3" is a fact; whether that was the module tree, the wave
/// schedule or luck is not something the log can know. Scoring a run good or
/// bad would need weights nobody derived from data — the failure docs/audit.md
/// already records — so the judgement stays with the reader and the exit code
/// stays 0.
#[derive(Debug, Serialize)]
pub struct Comparison {
    pub baseline: String,
    /// The protocol era each side ran under, when both recorded one and they
    /// differ (pact-okz.1). Listed FIRST in the rendering because it is the
    /// interpretive key: two runs under different protocols are not a
    /// controlled comparison, and reading them as one is exactly the mistake
    /// that made 223 messages look like evidence agents message voluntarily.
    pub protocol_shift: Option<(String, String)>,
    pub movements: Vec<Movement>,
    /// Unchanged fields, counted rather than listed — a report that prints
    /// forty "0" rows buries the three that moved.
    pub unchanged: usize,
    /// Fields the baseline does not carry at all, because it was written by an
    /// older pact. Named, never rendered as a delta from zero: "uncovered
    /// commits went from 0 to 19" would be a fabricated finding when the
    /// baseline simply predates the check.
    pub not_comparable: Vec<&'static str>,
}

/// How a comparable field is read out of an export document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extract {
    /// A number at the pointer. Absent means the baseline predates it.
    Number,
    /// An array at the pointer; its length is the value. Absent means the
    /// baseline predates it; empty means it found nothing.
    Len,
    /// A key in a map that only carries what actually occurred, such as
    /// `by_kind`. **Absent means zero, not unrecorded** — `by_kind` lists
    /// only the kinds a run produced, so a missing `refused` is a run with no
    /// contention, and calling that "the baseline is too old" would tell a
    /// reader to go and upgrade when the honest answer is "nothing was
    /// refused". Only the containing map going missing is "not comparable".
    SparseCount,
}

/// Every field compared, as (label, JSON pointer, how to read it).
///
/// A fixed list rather than a structural diff of the two documents: a
/// structural diff would report movement in timestamps, agent names and paths,
/// which is noise, and would silently start comparing any field added later
/// without anyone deciding it was comparable.
const COMPARED: &[(&str, &str, Extract)] = &[
    ("events", "/summary/events", Extract::Number),
    ("agents", "/summary/agents", Extract::Len),
    (
        "completed holds",
        "/summary/hold_secs/completed",
        Extract::Number,
    ),
    (
        "hold median (s)",
        "/summary/hold_secs/median_secs",
        Extract::Number,
    ),
    (
        "hold p90 (s)",
        "/summary/hold_secs/p90_secs",
        Extract::Number,
    ),
    (
        "hold max (s)",
        "/summary/hold_secs/max_secs",
        Extract::Number,
    ),
    ("open holds", "/summary/open_holds", Extract::Number),
    ("steals (forced)", "/summary/steals", Extract::Number),
    ("reclaims (expired)", "/summary/reclaims", Extract::Number),
    (
        "refusals (contention)",
        "/summary/by_kind/refused",
        Extract::SparseCount,
    ),
    ("watches active", "/summary/watches_active", Extract::Number),
    (
        "diffs delivered",
        "/summary/diffs_delivered",
        Extract::Number,
    ),
    (
        "deliveries failed",
        "/summary/deliveries_failed",
        Extract::Number,
    ),
    ("double-wins", "/double_win/double_wins", Extract::Len),
    ("stale holds", "/stale_holds/stale_holds", Extract::Len),
    (
        "chain breaks",
        "/chain_integrity/chain_breaks",
        Extract::Len,
    ),
    (
        "concurrent writes",
        "/commit_correlation/concurrent_writes",
        Extract::Len,
    ),
    (
        "uncovered commits",
        "/commit_correlation/uncovered_commits",
        Extract::Len,
    ),
    (
        "cross-held commits",
        "/commit_correlation/cross_held_commits",
        Extract::Len,
    ),
    (
        "holds with no commit",
        "/commit_correlation/holds_with_no_commit",
        Extract::Len,
    ),
    (
        "merge divergences",
        "/merge_divergence/merge_divergences",
        Extract::Len,
    ),
    (
        "claim/lease divergences",
        "/claim_lease_divergence/claim_divergences",
        Extract::Len,
    ),
    (
        "unacknowledged messages",
        "/unacknowledged_messages",
        Extract::Len,
    ),
];

fn extract(doc: &serde_json::Value, pointer: &str, how: Extract) -> Option<i64> {
    match how {
        Extract::Number => doc.pointer(pointer)?.as_i64(),
        // A missing array and an empty one are different: the first means the
        // baseline predates the field, the second means it found nothing.
        Extract::Len => doc.pointer(pointer)?.as_array().map(|a| a.len() as i64),
        Extract::SparseCount => {
            let (parent, _) = pointer.rsplit_once('/')?;
            // The map itself must exist for absence of the key to mean zero.
            doc.pointer(parent)?.as_object()?;
            Some(doc.pointer(pointer).and_then(|v| v.as_i64()).unwrap_or(0))
        }
    }
}

/// Compare `baseline` (a previously written `--export` document) against this
/// repository's current state.
pub fn compare(
    repo_root: &std::path::Path,
    baseline_path: &std::path::Path,
    since: Option<DateTime<Utc>>,
    include_annotated: bool,
) -> Result<Comparison> {
    let text = std::fs::read_to_string(baseline_path)
        .with_context(|| format!("reading baseline {}", baseline_path.display()))?;
    let baseline: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing baseline {}", baseline_path.display()))?;
    let current = serde_json::to_value(export(repo_root, since, include_annotated)?)?;

    let mut movements = Vec::new();
    let mut not_comparable = Vec::new();
    let mut unchanged = 0usize;
    for (field, pointer, how) in COMPARED {
        let now = extract(&current, pointer, *how).unwrap_or(0);
        match extract(&baseline, pointer, *how) {
            None => not_comparable.push(*field),
            Some(was) if was == now => unchanged += 1,
            Some(was) => movements.push(Movement {
                field,
                baseline: was,
                current: now,
                delta: now - was,
            }),
        }
    }

    // The dominant era on each side. A window spanning a protocol change is
    // already called out by the summary itself; here the question is only
    // whether the two runs are comparable at all.
    let era = |doc: &serde_json::Value| -> Option<String> {
        let map = doc.pointer("/summary/by_protocol")?.as_object()?;
        map.iter()
            .filter(|(k, _)| k.as_str() != "unknown")
            .max_by_key(|(_, v)| v.as_i64().unwrap_or(0))
            .map(|(k, _)| k.clone())
    };
    let protocol_shift = match (era(&baseline), era(&current)) {
        (Some(a), Some(b)) if a != b => Some((a, b)),
        _ => None,
    };

    Ok(Comparison {
        baseline: baseline_path.display().to_string(),
        protocol_shift,
        movements,
        unchanged,
        not_comparable,
    })
}

pub fn render_comparison(c: &Comparison) -> String {
    let mut out = vec![format!("compared against {}", c.baseline)];
    if let Some((was, now)) = &c.protocol_shift {
        out.push(String::new());
        out.push(format!(
            "PROTOCOL CHANGED between these runs: {was} -> {now}.\n\
             They are not a controlled comparison — anything below may be the \n\
             protocol rather than the fleet."
        ));
    }
    if c.movements.is_empty() {
        out.push(String::new());
        out.push(format!(
            "nothing moved ({} field(s) identical)",
            c.unchanged
        ));
    } else {
        out.push(String::new());
        out.push(format!(
            "{:<26} {:>10} {:>10} {:>10}",
            "FIELD", "BASELINE", "NOW", "DELTA"
        ));
        for m in &c.movements {
            out.push(format!(
                "{:<26} {:>10} {:>10} {:>+10}",
                m.field, m.baseline, m.current, m.delta
            ));
        }
        out.push(String::new());
        out.push(format!("{} field(s) unchanged", c.unchanged));
    }
    if !c.not_comparable.is_empty() {
        out.push(String::new());
        out.push(format!(
            "not comparable — the baseline predates {}: {}",
            if c.not_comparable.len() == 1 {
                "this field"
            } else {
                "these fields"
            },
            c.not_comparable.join(", ")
        ));
    }
    out.push(String::new());
    out.push(
        "Movement, not a verdict: which direction is GOOD depends on what you changed \n\
         and why, which the log cannot know."
            .to_string(),
    );
    out.join("\n")
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
        let has_commit = by_path
            .get(h.path.as_str())
            .is_some_and(|v| v.iter().any(|c| c.at >= open && c.at <= close));
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
                match (a.steals, a.reclaims) {
                    (0, 0) => String::new(),
                    (0, r) => format!(", {r} reclaim(s)"),
                    (st, 0) => format!(", {st} steal(s)"),
                    (st, r) => format!(", {st} steal(s), {r} reclaim(s)"),
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
            out.push(format!(
                "  the Beads store could not be asked ({reason}) — claim-lease-divergence could \
                 not run"
            ));
            return out.join("\n");
        }
        out.push(format!(
            "  {} hold(s) named no bead in their note, so there was nothing to cross-check",
            r.holds_naming_no_bead
        ));
    }

    if r.check == "commit-correlation" {
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

    for c in &r.claim_divergences {
        out.push(String::new());
        out.push(format!(
            "CLAIM/LEASE DIVERGENCE on {}: held by {} for {}, which bd assigns to {} (line {}, \
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

    /// A content-hash-bearing event, for the merge-divergence walk.
    fn ev_hash(at: &str, agent: &str, kind: &str, path: &str, hash: &str) -> String {
        format!(
            r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}","content_hash":"{hash}"}}"#
        )
    }

    /// pact-mqw.3: the crucible shape. Two agents each correctly held one file at
    /// different times, each added a match arm to the SAME match statement on its
    /// own branch, and git merged both insertions cleanly because they were
    /// textually non-adjacent — duplicate arms, no conflict marker, caught by a
    /// later compile failure rather than by pact.
    ///
    /// Nothing in the lease log looks wrong, because nothing in the lease protocol
    /// WAS wrong. The evidence is entirely in the content hashes.
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
            invoked_from: None,
            scope: None,
            pact_version: None,
            content_hash: None,
            subscriber: None,
            message_id: None,
            protocol_hash: None,
            head: None,
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

    // ------------------------------------------------------------ topology

    fn ev_from(at: &str, agent: &str, kind: &str, path: &str, from: &str) -> String {
        format!(
            r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}","invoked_from":"{from}","scope":"shared"}}"#
        )
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

    #[test]
    fn topology_expectations_are_all_or_nothing() {
        let tmp = with_log(&[
            &ev_from("2026-08-01T10:00:00Z", "a", "acquired", "a.rs", "main"),
            &ev_from("2026-08-01T10:01:00Z", "b", "acquired", "b.rs", "wt-b"),
        ]);
        // A mixed run satisfies neither expectation — deliberately, because
        // any "mostly" rule needs a cutoff nobody derived from data.
        let worktrees =
            run_check(tmp.path(), Check::Topology(Expect::Worktrees), None, false).unwrap();
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
        let r = run_check(tmp.path(), Check::Topology(Expect::Worktrees), None, false).unwrap();
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
        for expect in [Expect::Worktrees, Expect::Main, Expect::Any] {
            let r = run_check(tmp.path(), Check::Topology(expect), None, false).unwrap();
            assert_eq!(
                r.findings(),
                0,
                "{expect:?} must not fail on a pre-stamping log"
            );
            assert_eq!(r.topology_unstamped, 2);
        }
    }

    #[test]
    fn expect_and_check_names_are_validated() {
        assert!(Check::parse("topology", Some("worktrees")).is_ok());
        assert!(
            Check::parse("topology", None).is_ok(),
            "bare topology means `any`"
        );
        let bad = Check::parse("topology", Some("mostly"))
            .unwrap_err()
            .to_string();
        assert!(bad.contains("worktrees, main or any"), "{bad}");
        let unknown = Check::parse("nope", None).unwrap_err().to_string();
        assert!(
            unknown.contains("topology"),
            "the list must name it: {unknown}"
        );
    }

    fn ev_meta(at: &str, agent: &str, kind: &str, path: &str, extra: &str) -> String {
        format!(r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}"{extra}}}"#)
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

    // ------------------------------------------------- commit-correlation
    //
    // `with_log`'s `.git` is a bare, empty directory — enough to satisfy
    // `find_repo_root`, but not a real git repository `git log` can read.
    // `Check::CommitCorrelation` needs a REAL repository, so these tests get
    // their own fixture that actually runs `git init` and `git commit`.

    fn with_git_log(lines: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
        let pact = tmp.path().join(".pact");
        std::fs::create_dir_all(&pact).unwrap();
        std::fs::write(pact.join("events.jsonl"), lines.join("\n")).unwrap();
        tmp
    }

    /// Writes (or rewrites) `file` and commits it under `at`, an RFC3339
    /// timestamp shared by author and committer date so the fixture's
    /// commits line up exactly with the hand-written event timestamps above
    /// them.
    fn git_commit(repo: &std::path::Path, file: &str, at: &str) {
        std::fs::write(repo.join(file), at).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
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
        assert!(run(&["add", file]).success());
        assert!(run(&["commit", "--quiet", "-m", &format!("touch {file}")]).success());
    }

    /// [`git_commit`] with a `Pact-Agent` trailer, so a fixture can say which
    /// agent made a commit — which is the fact git itself cannot supply, since
    /// every agent in every fleet so far commits under one git identity.
    fn git_commit_as(repo: &std::path::Path, file: &str, at: &str, agent: &str) {
        if let Some(parent) = repo.join(file).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(repo.join(file), format!("{at} {agent}")).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
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
        assert!(run(&["add", file]).success());
        assert!(run(&[
            "commit",
            "--quiet",
            "-m",
            &format!("touch {file}"),
            "--trailer",
            &format!("Pact-Agent={agent}"),
        ])
        .success());
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

    // ---------------------------------------------------------- comparison

    fn write_baseline(dir: &std::path::Path, report: &ExportReport) -> std::path::PathBuf {
        let p = dir.join("baseline.json");
        std::fs::write(&p, serde_json::to_string(report).unwrap()).unwrap();
        p
    }

    #[test]
    fn comparing_a_report_to_itself_reports_no_movement() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        let base = write_baseline(tmp.path(), &export(tmp.path(), None, false).unwrap());
        let c = compare(tmp.path(), &base, None, false).unwrap();
        assert!(c.movements.is_empty(), "{:?}", c.movements);
        assert!(c.not_comparable.is_empty(), "{:?}", c.not_comparable);
        assert!(c.unchanged > 0);
        assert!(render_comparison(&c).contains("nothing moved"));
    }

    #[test]
    fn a_run_that_moved_names_each_field_and_by_how_much() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        let base = write_baseline(tmp.path(), &export(tmp.path(), None, false).unwrap());

        // A second, longer hold by a second agent.
        let log = tmp.path().join(".pact").join("events.jsonl");
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push('\n');
        text.push_str(&ev("2026-08-01T11:00:00Z", "b", "acquired", "b.rs"));
        text.push('\n');
        text.push_str(&ev("2026-08-01T11:30:00Z", "b", "released", "b.rs"));
        std::fs::write(&log, text).unwrap();

        let c = compare(tmp.path(), &base, None, false).unwrap();
        let moved: BTreeMap<&str, i64> = c.movements.iter().map(|m| (m.field, m.delta)).collect();
        assert_eq!(moved.get("events"), Some(&2), "{moved:?}");
        assert_eq!(moved.get("agents"), Some(&1), "{moved:?}");
        assert_eq!(moved.get("completed holds"), Some(&1), "{moved:?}");
        assert!(moved.contains_key("hold max (s)"), "{moved:?}");

        let text = render_comparison(&c);
        assert!(text.contains("events"), "{text}");
        // Never a verdict.
        assert!(text.contains("Movement, not a verdict"), "{text}");
    }

    /// The acceptance case that keeps this honest: an older pact's export has
    /// no `unacknowledged_messages` at all, and reporting "0 -> 3" would be a
    /// fabricated finding rather than a measurement.
    #[test]
    fn a_baseline_missing_a_field_is_not_comparable_rather_than_a_delta_from_zero() {
        let tmp = with_log(&[&ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs")]);
        let mut doc = serde_json::to_value(export(tmp.path(), None, false).unwrap()).unwrap();
        // Simulate an older export: drop two whole sections.
        doc.as_object_mut()
            .unwrap()
            .remove("unacknowledged_messages");
        doc.as_object_mut().unwrap().remove("commit_correlation");
        let base = tmp.path().join("old.json");
        std::fs::write(&base, serde_json::to_string(&doc).unwrap()).unwrap();

        let c = compare(tmp.path(), &base, None, false).unwrap();
        assert!(
            c.not_comparable.contains(&"unacknowledged messages"),
            "{:?}",
            c.not_comparable
        );
        assert!(
            c.not_comparable.contains(&"uncovered commits"),
            "{:?}",
            c.not_comparable
        );
        assert!(
            !c.movements.iter().any(|m| m.field == "uncovered commits"),
            "a missing baseline field must not produce a delta: {:?}",
            c.movements
        );
        assert!(render_comparison(&c).contains("baseline predates"));
    }

    /// `by_kind` lists only the kinds that occurred, so an absent `refused`
    /// means a run with no contention — NOT a baseline too old to say. Getting
    /// this backwards tells a reader to upgrade when the honest answer is
    /// "nothing was refused".
    #[test]
    fn an_absent_sparse_count_reads_as_zero_not_as_unrecorded() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        let base = write_baseline(tmp.path(), &export(tmp.path(), None, false).unwrap());
        assert!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&base).unwrap())
                .unwrap()
                .pointer("/summary/by_kind/refused")
                .is_none(),
            "fixture must genuinely lack the key"
        );

        let log = tmp.path().join(".pact").join("events.jsonl");
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push('\n');
        text.push_str(&ev("2026-08-01T11:00:00Z", "b", "refused", "a.rs"));
        std::fs::write(&log, text).unwrap();

        let c = compare(tmp.path(), &base, None, false).unwrap();
        assert!(
            !c.not_comparable.contains(&"refusals (contention)"),
            "absent means zero here: {:?}",
            c.not_comparable
        );
        let m = c
            .movements
            .iter()
            .find(|m| m.field == "refusals (contention)")
            .expect("it moved from 0 to 1");
        assert_eq!((m.baseline, m.current, m.delta), (0, 1, 1));
    }

    /// pact-okz.1 feeding pact-okz.2: two runs under different protocol
    /// revisions are not a controlled comparison, and the report has to say so
    /// before anything else — reading them as one is the mistake that made 223
    /// messages look like evidence agents message voluntarily.
    #[test]
    fn a_protocol_shift_between_the_two_runs_is_called_out_first() {
        let tmp = with_log(&[&ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs")]);
        let mut doc = serde_json::to_value(export(tmp.path(), None, false).unwrap()).unwrap();
        doc.pointer_mut("/summary")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(
                "by_protocol".to_string(),
                serde_json::json!({ "aaaaaaaa": 1 }),
            );
        let base = tmp.path().join("old-protocol.json");
        std::fs::write(&base, serde_json::to_string(&doc).unwrap()).unwrap();

        // Give the current side a different era.
        let log = tmp.path().join(".pact").join("events.jsonl");
        std::fs::write(
            &log,
            r#"{"at":"2026-08-01T10:00:00Z","agent":"a","kind":"acquired","path":"a.rs","protocol_hash":"bbbbbbbb"}"#,
        )
        .unwrap();

        let c = compare(tmp.path(), &base, None, false).unwrap();
        assert_eq!(
            c.protocol_shift,
            Some(("aaaaaaaa".to_string(), "bbbbbbbb".to_string()))
        );
        let text = render_comparison(&c);
        assert!(text.contains("PROTOCOL CHANGED"), "{text}");
        assert!(text.contains("not a controlled comparison"), "{text}");
    }

    // ------------------------------------------------------------- export

    #[test]
    fn export_combines_the_summary_every_check_and_doctor() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        let report = export(tmp.path(), None, false).unwrap();
        assert_eq!(report.summary.events, 2);
        assert_eq!(report.double_win.check, "double-win");
        assert_eq!(report.stale_holds.check, "stale-holds");
        assert_eq!(report.chain_integrity.check, "chain-integrity");
        assert_eq!(report.commit_correlation.check, "commit-correlation");
        assert!(
            !report.doctor.checks.is_empty(),
            "doctor's checks must ride along, not just its healthy flag"
        );

        // The whole point is a file another agent session can read directly —
        // pin that it actually round-trips through serde as one JSON object,
        // not just that the Rust struct is well-formed.
        let json = serde_json::to_value(&report).unwrap();
        for key in [
            "summary",
            "double_win",
            "stale_holds",
            "chain_integrity",
            "commit_correlation",
            "doctor",
            "observations",
        ] {
            assert!(json.get(key).is_some(), "missing {key} in exported JSON");
        }
    }

    /// The `observations` list is what saves a reader from re-deriving
    /// "worth a look" from raw counts — a stale hold must actually show up
    /// there, in a form that names what it is.
    #[test]
    fn export_observations_name_a_real_stale_hold() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "slow", "acquired", "src/slow.rs"),
            &ev("2026-08-01T12:00:00Z", "slow", "released", "src/slow.rs"),
        ]);
        let report = export(tmp.path(), None, false).unwrap();
        assert_eq!(report.stale_holds.findings(), 1);
        assert!(
            report.observations.iter().any(|o| o.contains("stale hold")),
            "{:?}",
            report.observations
        );
    }

    /// A clean lease/commit history must not manufacture an audit-side
    /// observation when nothing rose to that bar — only `doctor`'s own
    /// checks (a separate concern, covered by its own test suite) may still
    /// contribute to the list for this fixture.
    #[test]
    fn export_adds_no_audit_observation_when_the_history_is_clean() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:00:30+00:00");

        let report = export(tmp.path(), None, false).unwrap();
        assert_eq!(report.double_win.findings(), 0);
        assert_eq!(report.stale_holds.findings(), 0);
        assert_eq!(report.chain_integrity.findings(), 0);
        assert_eq!(report.commit_correlation.findings(), 0);
        assert!(
            report.observations.iter().all(|o| o.starts_with("doctor:")),
            "a clean history must not add any non-doctor observation: {:?}",
            report.observations
        );
    }
}
