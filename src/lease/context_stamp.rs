//! What a lease transition is stamped with, and where that stamp is recorded:
//! the branch/worktree/invocation triple a lock carries, the event row
//! [`log_event`] appends for every transition, and the OpenTelemetry counters
//! and histograms that sit beside it.
//!
//! All three answer one question — *where was the holder, and what did they
//! do* — and they are here together because they must not disagree. The
//! worktree triple is decided once so a lock and its events cannot record
//! different places; every metric sits next to the `log_event` for the same
//! transition so the feed and the metric cannot diverge; and an expiry carries
//! the HOLDER's context, never the sweeper's.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::events;
use crate::otel;

use super::*;

#[cfg(feature = "otel")]
use crate::repo::pact_dir;
#[cfg(feature = "otel")]
use std::path::PathBuf;

/// The branch/worktree pair to stamp on a new lease, empty unless this
/// repository actually has linked worktrees.
///
/// Keyed on `has_worktrees` rather than on "am I in a linked worktree": the
/// holder may be sitting in the MAIN worktree while the loser is in a linked
/// one, and a conflict message that can only name the loser's worktree explains
/// nothing.
/// The three facts a lock records about WHERE its holder was: branch, worktree name, and
/// the invocation point `Event::invoked_from` uses.
///
/// All three are gated on the same condition and decided here, so they cannot disagree
/// about whether this repository has worktrees at all.
///
/// **Absent in a repo with no linked worktrees, deliberately.** Lock files stay
/// byte-identical to what pact wrote before it understood worktrees, which two worktree
/// tests assert directly. Nothing is lost by omitting `invoked_from` there: the finding it
/// exists for (pact-83r.3 / finding 5a) is that an expiry inherits the SWEEPER's location
/// rather than the holder's, and in a single checkout those are necessarily the same place.
pub(super) fn worktree_stamp(repo_root: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let ctx = crate::repo::RepoContext::resolve(repo_root);
    if !ctx.has_worktrees {
        return (None, None, None);
    }
    (
        ctx.branch(),
        ctx.worktree_name.clone(),
        Some(crate::repo::invoked_from(&ctx)),
    )
}

/// "held by agent-a", plus where they are holding it from when that is knowable.
///
/// Cross-worktree contention is confusing in a way same-directory contention is
/// not: the loser cannot see the file changing, because the holder is editing a
/// different checkout of it. Saying only "held by agent-a" invites the reader to
/// go look at their own working copy, find it untouched, and conclude the lease
/// is stale.
pub(super) fn holder_location(lease: &LeaseInfo) -> String {
    match (&lease.branch, &lease.worktree) {
        (Some(b), Some(w)) => format!("{} on branch {b} in worktree {w}", lease.agent),
        (Some(b), None) => format!("{} on branch {b}", lease.agent),
        (None, Some(w)) => format!("{} in worktree {w}", lease.agent),
        (None, None) => lease.agent.clone(),
    }
}

/// Warn when the copy being leased is not the copy the last holder left.
///
/// **A lease is exclusive in TIME. It is not exclusive across COPIES.** In one
/// shared checkout that distinction does not exist — the file has a single state
/// and the second writer sees the first writer's bytes. Under the
/// branch-per-agent worktree topology, which pact explicitly supports and every
/// field run so far has used, it does:
///
/// ```text
/// agent A  acquires, edits, commits to branch A, releases      (compliant)
/// agent B  acquires, edits a DIFFERENT COPY on branch B that
///          never contained A's change, commits, releases       (compliant)
/// ```
///
/// Both leases were honoured. The conflict is deferred to a merge performed later,
/// by someone else, with no lease held by anyone and pact not involved — and the
/// merge window is where the corruption lands. Three instances in one 85-minute
/// crucible run: duplicate match arms in src/printer.rs, six duplicate test
/// functions in src/parser.rs (E0428), and a near-miss where a successor found
/// that applying a stashed diff would have silently reverted a peer's `Expr::If`.
/// **No conflict marker was ever produced** — git merged both insertions cleanly
/// because they were textually non-adjacent, which is exactly the edit shape this
/// topology encourages and a designed hot file attracts.
///
/// pact cannot fix git. It can convert a silent deferred hazard into a loud
/// warning at the one moment it is cheap to act on, which is now.
///
/// Advisory: warns on stderr, never fails the acquire, and stays silent whenever
/// it cannot tell — no prior release on record, a file that does not exist yet, a
/// release logged before content hashes were stamped. A false alarm on a first
/// acquire would train agents to ignore the real one.
pub(super) fn warn_if_copy_diverged(
    relative: &str,
    mine: Option<&str>,
    left: Option<&events::ReleasedContent>,
) {
    let Some(mine) = mine else { return };
    let Some(left) = left else { return };
    if left.hash == mine {
        return;
    }
    crate::output::warn(&format!(
        "warning: your copy of {relative} is NOT the copy {} left when they released it at {} \
         — a lease is exclusive in time, not across worktrees, so you may be about to edit a \
         branch that never contained their change. git will often merge both edits with no \
         conflict marker. Reconcile first (`git merge`/`git rebase`, or `pact msg send \
         --to-owner-of {relative}`) rather than at merge time, when both plans are sunk cost",
        left.agent, left.at
    ));
}

/// Record a lease transition in the activity log (pact-rnc.13). Releasing a
/// lease deletes the only record that it ever existed, so lease history cannot
/// be reconstructed after the fact — it has to be written as it happens.
///
/// Infallible on purpose: `events::append` swallows its own I/O errors, and
/// this returns `()`, so no lease operation can ever fail *because* logging
/// failed. A missing line in the feed is cheaper than a refused claim.
/// `ttl_secs` is the TTL of the lease this event is ABOUT — the incoming holder's
/// for an acquire or steal, the departing holder's for a release or expiry.
///
/// Recorded because `pact audit --check stale-holds` has to judge a hold against
/// the TTL that was in force when it was taken, not against whatever the binary
/// happens to be compiled with. Without it, raising the default silently
/// reclassifies history: the 900s-era holds in this repo top out at 36m, so under
/// a 45m default every one of them would quietly stop being a finding.
// Eight positional arguments, and a struct would be worse here: this has eleven callers
// and every one of them passes a different combination, so a builder or an args struct
// would move the same noise to eleven call sites and add a type with no other purpose.
// Same reasoning `msg::create` recorded before it was deleted.
#[allow(clippy::too_many_arguments)]
pub(super) fn log_event(
    repo_root: &Path,
    agent: &str,
    kind: &str,
    path: &str,
    detail: Option<String>,
    ttl_secs: u64,
    // Only ever `Some` for the kinds that OPEN a hold — see
    // `Event::content_hash`. Threaded as a parameter rather than recomputed
    // here so the value written to the event is byte-identical to the one
    // written to the lock file: two hashes of the same path taken moments
    // apart could legitimately differ, and a diff computed against a baseline
    // the log does not agree with would be unexplainable afterwards.
    content_hash: Option<String>,
    // The HOLDER's invocation context, for a row somebody else is writing on their
    // behalf. Only `collect_expired` passes it: an expiry is a fact about the holder's
    // lease, and `events::append` would otherwise stamp whoever swept the lock. `None`
    // everywhere else means "stamp my own", which is right for every event an agent
    // writes about itself (pact-83r.3 / finding 5).
    holder_invoked_from: Option<String>,
) {
    events::append(
        repo_root,
        &events::Event {
            at: Utc::now().to_rfc3339(),
            agent: agent.to_string(),
            kind: kind.to_string(),
            path: Some(path.to_string()),
            detail,
            // Lease events never annotate; only a hand-written
            // correction does. See audit::ANNOTATION_KIND.
            ttl_secs: Some(ttl_secs),
            covers_lines: None,
            actor: None,
            // Every force-released event that needs this bypasses log_event
            // entirely (see the call site in release_fs) — nothing routed
            // through here ever has a displaced holder to name.
            displaced: None,
            // append() computes the real value from the log; see
            // Event::chain_hash (pact-m7j.2.5).
            chain_hash: None,
            // Likewise stamped by append(), which is the only place that can
            // measure them; see Event::invoked_from (pact-ler.1) — unless the caller
            // knows better, which only the expiry sweeper does.
            invoked_from: holder_invoked_from,
            collected_from: None,
            scope: None,
            pact_version: None,
            content_hash,
            subscriber: None,
            message_id: None,
            protocol_hash: None,
            head: None,
            holder: None,
            holder_remaining_secs: None,
            holder_branch: None,
            holder_worktree: None,
            ..Default::default()
        },
    );
}

// ---------------------------------------------------------------------------
// Telemetry (pact-aw7.3). The retro counted 19 acquires / 19 releases / 0
// steals by hand-scanning `pact log`; these make it a query. Every one of them
// sits next to the `log_event` for the same transition, so the feed and the
// metric cannot disagree about what happened.
//
// WHERE A NAME IS AND IS NOT. Neither `pact.path` nor an agent name is on a
// metric; both are on the SPAN. A metric attribute has to be bounded, and
// neither of these is: a repo has thousands of files, and an agent name is
// whatever `PACT_AGENT` was set to. `pact.lease.peer` used to dimension all
// three metrics here, and a verifier measured what that costs — five agents
// doing one operation each produced ten distinct series, this repo's own
// `lease ls --all` already lists sixteen historical agents, every fleet mints
// more, and nothing ages a series out. So: the metric tells you the *rate* of
// each outcome, and the trace tells you *which file* and *which peer* — click
// through, don't group by. `pact log` and `.pact/events.jsonl` still carry the
// full who-blocked-whom edge, which is where it was before the metrics existed.
//
// None of this may change an exit code or write to stdout: with the `otel`
// feature off every call below compiles to nothing, and with it on the
// exporter is fire-and-forget (see src/otel.rs).
// ---------------------------------------------------------------------------

/// Longest agent name we write into a wait marker. An agent name arrives from
/// `PACT_AGENT` or from a lock file another process wrote — neither is a
/// promise about length.
#[cfg(feature = "otel")]
const MAX_AGENT_LEN: usize = 64;

/// A conflict older than this tells you nothing about a wait: the agent went
/// away and came back, or the marker outlived the run that left it.
#[cfg(feature = "otel")]
const MAX_WAIT_MS: f64 = 6.0 * 3600.0 * 1000.0;

#[cfg(feature = "otel")]
fn short_agent(agent: &str) -> String {
    agent.chars().take(MAX_AGENT_LEN).collect()
}

/// One counter for every lease transition, dimensioned by outcome and by
/// nothing else. See the section header for why the peer is not here.
///
/// `reclaimed` and `stolen` are separate outcomes here even though the event
/// log writes both as `"stolen"`. Taking over a dead claim is not overriding a
/// live one, and in the feed the only thing that distinguishes them is free
/// text in `detail`, which nobody can group by.
pub(super) fn count_transition(outcome: &'static str) {
    otel::count(
        "pact.lease.transitions",
        1,
        &otel::attrs!["pact.lease.outcome" => outcome],
    );
}

/// Milliseconds a lease had been held, or `None` when its `acquired_at` cannot
/// be parsed. [`parse_acquired`] answers epoch-0 for a corrupt stamp on purpose
/// — the safe answer for "is this reclaimable" — but feeding that to a
/// histogram would record a 56-year hold and drag every percentile with it.
fn held_ms(lease: &LeaseInfo, now: DateTime<Utc>) -> Option<f64> {
    let acquired = DateTime::parse_from_rfc3339(&lease.acquired_at).ok()?;
    let ms = (now - acquired.with_timezone(&Utc)).num_milliseconds();
    (ms >= 0).then_some(ms as f64)
}

/// Record a hold that just ended. `pact.lease.overrun` is the half of
/// pact-aw7.3 that says "a lease held past its TTL is visible as a metric
/// rather than only in `pact log`" — true when the claim outlived the TTL its
/// holder promised, which until now you could only see by reading the feed.
///
/// Caveat worth knowing before you read the percentiles: `renew` resets
/// `acquired_at`, so a renewed lease reports time-since-last-renew, not
/// time-since-first-claim. That is also exactly what `overrun` needs to mean —
/// a renewed lease has not broken its promise.
pub(super) fn record_hold(lease: &LeaseInfo, outcome: &'static str) {
    let now = Utc::now();
    let Some(ms) = held_ms(lease, now) else {
        return;
    };
    let (_, remaining) = age_and_remaining(lease, now);
    otel::record_ms(
        "pact.lease.hold.duration",
        ms,
        &otel::attrs![
            "pact.lease.outcome" => outcome,
            "pact.lease.overrun" => remaining < 0,
        ],
    );
}

/// Where a conflict leaves a breadcrumb so that a *later* acquire can say what
/// the conflict cost. The two events are different processes — pact exits
/// between them — so the gap cannot be measured in memory, and it is not
/// derivable from `.pact/events.jsonl` either: a refused acquire writes no
/// event, and adding one would make the *blocked* agent the answer to
/// `events::owner_of`, i.e. `msg send --to-owner-of` would start routing mail
/// to the agent that lost the file.
///
/// So: an empty-ish file whose mtime is the conflict and whose contents are the
/// agent that blocked us. Invisible to everything else — `scan` and
/// `corrupt_count` only look at `*.lock`, and this is a sibling directory.
///
/// Everything in this block is `#[cfg(feature = "otel")]` and has a no-op twin
/// below, the same shape otel.rs uses for its own API. It has to be: only the
/// terminal `otel::record_ms` compiled away, so the DEFAULT build — telemetry
/// compiled out, no `OTEL_*` in the environment — created `.pact/waits/` on
/// every acquire and wrote a marker on every conflict, to feed a histogram it
/// can never emit. The section header three screens up claims "with the `otel`
/// feature off every call below compiles to nothing"; these attributes are what
/// make that true.
///
/// The leak was the default outcome of a conflict, not an edge case: a marker
/// is only collected when the *same* agent later acquires the *same* path,
/// which is precisely what AGENTS.md tells a blocked agent not to do ("message
/// them and pick up something else"). Hence [`sweep_wait_markers`].
#[cfg(feature = "otel")]
fn wait_marker(repo_root: &Path, agent: &str, relative: &str) -> Option<PathBuf> {
    let dir = pact_dir(repo_root).ok()?.join("waits");
    std::fs::create_dir_all(&dir).ok()?;
    // Same encoding, and the same collision caveat, as `encode_path`.
    Some(dir.join(format!(
        "{}__{}.wait",
        encode_path(agent),
        encode_path(relative)
    )))
}

#[cfg(feature = "otel")]
pub(super) fn mark_conflict(repo_root: &Path, agent: &str, relative: &str, blocker: &str) {
    if let Some(marker) = wait_marker(repo_root, agent, relative) {
        // The blocker's name is written but never exported: it is what makes a
        // marker readable when someone goes looking in `.pact/waits/`, and an
        // agent name has no business being a metric dimension (section header).
        let _ = std::fs::write(marker, short_agent(blocker));
    }
}

/// Consume the breadcrumb from [`mark_conflict`] and record how long the agent
/// was locked out of this path.
#[cfg(feature = "otel")]
pub(super) fn record_wait(repo_root: &Path, agent: &str, relative: &str) {
    let Some(marker) = wait_marker(repo_root, agent, relative) else {
        return;
    };
    if std::fs::read(&marker).is_err() {
        return; // no conflict preceded this acquire
    }
    let elapsed = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok());
    let _ = std::fs::remove_file(&marker);
    let Some(elapsed) = elapsed else { return };
    let ms = elapsed.as_secs_f64() * 1000.0;
    if ms > MAX_WAIT_MS {
        return;
    }
    otel::record_ms("pact.lease.wait.duration", ms, &otel::attrs![]);
}

/// Drop every marker this agent left behind. `release --all` is the one call
/// that means "I am done here", so it is the only place that can honestly say a
/// pending wait will never be collected — and without it a conflict the agent
/// never retried leaks one ~16-byte file per (agent, path) forever.
#[cfg(feature = "otel")]
pub(super) fn sweep_wait_markers(repo_root: &Path, agent: &str) {
    let Ok(dir) = pact_dir(repo_root).map(|d| d.join("waits")) else {
        return;
    };
    let prefix = format!("{}__", encode_path(agent));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// The no-op twins. Identical signatures, so no call site carries a `#[cfg]`.
#[cfg(not(feature = "otel"))]
#[inline(always)]
pub(super) fn mark_conflict(_repo_root: &Path, _agent: &str, _relative: &str, _blocker: &str) {}
#[cfg(not(feature = "otel"))]
#[inline(always)]
pub(super) fn record_wait(_repo_root: &Path, _agent: &str, _relative: &str) {}
#[cfg(not(feature = "otel"))]
#[inline(always)]
pub(super) fn sweep_wait_markers(_repo_root: &Path, _agent: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::testutil::*;

    #[test]
    fn each_lease_operation_appends_its_event() {
        let tmp = repo();
        let root = tmp.path();

        acquire(root, "agent-a", "f.rs", 900, false, Some("why".into())).unwrap();
        renew(root, "agent-a", "f.rs").unwrap();
        acquire(root, "agent-a", "f.rs", 900, false, None).unwrap(); // re-entrant
        acquire(root, "agent-b", "f.rs", 900, true, None).unwrap(); // steal
        release(root, "agent-a", "f.rs", true).unwrap(); // force-release
        acquire(root, "agent-a", "g.rs", 900, false, None).unwrap();
        release(root, "agent-a", "g.rs", false).unwrap();

        assert_eq!(
            event_kinds(root),
            vec![
                ("acquired".to_string(), "f.rs".to_string()),
                ("renewed".to_string(), "f.rs".to_string()),
                ("renewed".to_string(), "f.rs".to_string()),
                // The steal closes agent-a's window before opening agent-b's
                // (pact-mqw.1); without the first row agent-a reads as still
                // holding f.rs for the rest of the log.
                ("stolen".to_string(), "f.rs".to_string()),
                ("displaced".to_string(), "f.rs".to_string()),
                ("force-released".to_string(), "f.rs".to_string()),
                ("acquired".to_string(), "g.rs".to_string()),
                ("released".to_string(), "g.rs".to_string()),
            ]
        );

        let events = crate::events::recent(root, 100).unwrap();
        assert_eq!(events[0].agent, "agent-a");
        assert_eq!(
            events[0].detail.as_deref(),
            Some("why"),
            "acquire logs the note — the reason the claim exists"
        );
        // pact-mqw.1: the steal is two rows, matching the expired-reclaim branch
        // — one closing the victim's window under THEIR name, one opening the
        // thief's under theirs.
        assert_eq!(
            events[3].agent, "agent-b",
            "the steal opens the thief's window"
        );
        assert!(
            events[3].detail.as_deref().unwrap().contains("agent-a"),
            "a steal must name the displaced holder: {:?}",
            events[3].detail
        );
        assert_eq!(events[4].kind, "displaced");
        assert_eq!(
            events[4].agent, "agent-a",
            "the victim owns the closing row"
        );
        assert!(
            events[4].detail.as_deref().unwrap().contains("agent-b"),
            "the displaced row must name who overrode it: {:?}",
            events[4].detail
        );
        assert!(
            events[5].detail.as_deref().unwrap().contains("agent-b"),
            "a force-release must name the displaced holder: {:?}",
            events[5].detail
        );
        // pact-m7j.2.6: the displaced holder in a structured field too, not
        // just free text — audit::reconstruct needs this to close THEIR
        // window, since this event's own `agent` (agent-a) is the one who
        // forced it, not the one displaced.
        assert_eq!(
            events[5].displaced.as_deref(),
            Some("agent-b"),
            "force-released must carry the displaced holder as a structured field: {:?}",
            events[5].displaced
        );
    }

    /// pact-juz.1: a denied acquire used to leave nothing in
    /// `.pact/events.jsonl` at all — only a throwaway `.pact/waits/` marker
    /// (excluded from `pact audit`'s history by design) and an OTEL counter
    /// compiled out entirely without `--features otel`. Reproduced live on a
    /// real 15-agent build where paths with 6-8 distinct holders showed zero
    /// contention in `pact log`, because there was nothing to show either
    /// way. `refused` closes that gap without opening or closing any hold
    /// window — see `audit::tests` for the reconstruct-side half of this.
    /// pact-ler.1: the invocation context is stamped in `events::append`, the
    /// one funnel every kind passes through — so it must be on kinds that do
    /// NOT go through `log_event`'s common path too. `refused` is written from
    /// the conflict branch and `force-released` bypasses `log_event` entirely
    /// to set `displaced`; both were exactly the kind of call site that let
    /// `branch`/`worktree` end up conditional in the first place.
    #[test]
    fn every_event_kind_carries_the_invocation_context_not_just_the_common_path() {
        let tmp = repo();
        let root = tmp.path();

        acquire(root, "agent-a", "hot.rs", 900, false, None).unwrap();
        // refused: the conflict branch.
        assert!(acquire(root, "agent-b", "hot.rs", 900, false, None).is_err());
        // force-released: bypasses log_event to name the displaced holder.
        release(root, "agent-b", "hot.rs", true).unwrap();

        let events = crate::events::recent(root, 100).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert!(
            kinds.contains(&"refused") && kinds.contains(&"force-released"),
            "fixture must exercise both off-common-path kinds: {kinds:?}"
        );
        for e in &events {
            // `"outside"`, not `"main"`, and that is the correct answer rather
            // than a test artefact to work around: a unit test's CWD is pact's
            // own checkout while `root` is a tempdir, so the process really is
            // standing outside the repository it is writing leases for — the
            // same shape as driving pact at another repo via `PACT_STATE_DIR`,
            // and the one case where the lease/edit binding cannot be assumed.
            // The `"main"` and linked-worktree-name values are covered end to
            // end, under the real binary with a real CWD, by
            // `every_event_records_which_worktree_pact_was_invoked_from` in
            // tests/worktree.rs.
            assert_eq!(
                e.invoked_from.as_deref(),
                Some("outside"),
                "{} has no invoked_from",
                e.kind
            );
            assert_eq!(e.scope.as_deref(), Some("shared"), "{}", e.kind);
            assert_eq!(
                e.pact_version.as_deref(),
                Some(env!("CARGO_PKG_VERSION")),
                "{}",
                e.kind
            );
        }
    }

    /// A histogram must never be fed the epoch. `parse_acquired` answers
    /// epoch-0 for a corrupt stamp deliberately (it makes the lease
    /// reclaimable), and passing that through as a hold time would record a
    /// 56-year lease and drag every percentile with it.
    #[test]
    fn held_ms_measures_a_real_hold_and_refuses_a_corrupt_one() {
        let (lease, now) = lease_aged(900, 5);
        let ms = held_ms(&lease, now).expect("a parseable stamp has a hold time");
        assert!(
            (4_000.0..=6_000.0).contains(&ms),
            "got {ms}ms for a 5s hold"
        );

        let corrupt = LeaseInfo {
            acquired_at: "not-a-timestamp".into(),
            ..lease
        };
        assert_eq!(held_ms(&corrupt, now), None);
    }

    /// An agent name reaches a wait marker from `PACT_AGENT` or from a lock
    /// file another process wrote. Neither is a promise about length.
    #[cfg(feature = "otel")]
    #[test]
    fn short_agent_bounds_what_reaches_a_wait_marker() {
        assert_eq!(short_agent("lease-metrics"), "lease-metrics");
        assert_eq!(short_agent(&"x".repeat(500)).len(), MAX_AGENT_LEN);
    }

    /// The other side of the cfg, and the reason it was added: with telemetry
    /// compiled out, a conflict must leave NOTHING on disk. The default build
    /// created `.pact/waits/` on every acquire and a marker on every conflict —
    /// filesystem work whose only consumer was a histogram that build cannot
    /// emit, and which nothing ever collected, because a marker is only read
    /// when the same agent retries the same path and AGENTS.md tells a blocked
    /// agent to go do something else.
    #[cfg(not(feature = "otel"))]
    #[test]
    fn the_default_build_does_no_telemetry_filesystem_work() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "hot.rs");
        let waits = root.join(".pact").join("waits");
        assert!(!waits.exists(), "an uncontended acquire created {waits:?}");

        assert!(acquire(root, "agent-b", "hot.rs", 900, false, None).is_err());
        assert!(!waits.exists(), "a conflict created {waits:?}");
    }

    /// The conflict → acquire gap spans two processes, so it is measured off a
    /// breadcrumb on disk. The invariant that matters: a conflict leaves one
    /// naming the blocker, the acquire that finally succeeds consumes it, and
    /// an uncontended acquire leaves nothing behind.
    #[cfg(feature = "otel")]
    #[test]
    fn a_conflict_leaves_a_wait_marker_that_the_next_acquire_consumes() {
        let tmp = repo();
        let root = tmp.path();
        let marker = wait_marker(root, "agent-b", "hot.rs").unwrap();

        claim(root, "agent-a", "hot.rs");
        assert!(!marker.exists(), "no conflict yet");

        // agent-b is blocked: exit 2, and the breadcrumb names who blocked it.
        let err = acquire(root, "agent-b", "hot.rs", 900, false, None).unwrap_err();
        assert_eq!(
            crate::output::code_for(&err),
            2,
            "telemetry changed the exit code"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "agent-a");

        // …then gets the file, which closes the wait and clears the marker.
        release(root, "agent-a", "hot.rs", false).unwrap();
        acquire(root, "agent-b", "hot.rs", 900, false, None).unwrap();
        assert!(!marker.exists(), "the wait was recorded but not consumed");

        // An acquire nobody contended leaves no breadcrumb at all.
        acquire(root, "agent-b", "quiet.rs", 900, false, None).unwrap();
        assert!(!wait_marker(root, "agent-b", "quiet.rs").unwrap().exists());
    }

    /// A conflict the agent never retried used to leak its marker forever,
    /// because the only thing that collects one is the same agent acquiring the
    /// same path — the one move the protocol tells a blocked agent NOT to make.
    /// `release --all` is where an agent says it is done, so it is where the
    /// breadcrumbs go.
    #[cfg(feature = "otel")]
    #[test]
    fn release_all_sweeps_the_wait_markers_this_agent_left() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "hot.rs");
        assert!(acquire(root, "agent-b", "hot.rs", 900, false, None).is_err());
        let mine = wait_marker(root, "agent-b", "hot.rs").unwrap();
        let theirs = wait_marker(root, "agent-c", "hot.rs").unwrap();
        std::fs::write(&theirs, "agent-a").unwrap();
        assert!(mine.exists() && theirs.exists());

        // agent-b never retried hot.rs — it went and did something else, which
        // is exactly what the protocol asked of it.
        claim(root, "agent-b", "other.rs");
        release_all(root, "agent-b").unwrap();

        assert!(!mine.exists(), "release --all left {mine:?} behind");
        assert!(theirs.exists(), "swept another agent's marker");
    }

    /// A broken event log must not break a lease operation: `append` swallows
    /// its own errors, so `acquire` still succeeds with the log unwritable.
    #[test]
    fn a_failing_event_log_does_not_fail_the_lease() {
        let tmp = repo();
        let root = tmp.path();
        // A directory where the log file belongs: every append fails.
        std::fs::create_dir_all(root.join(".pact/events.jsonl")).unwrap();

        assert!(acquire(root, "agent-a", "f.rs", 900, false, None).is_ok());
        assert_eq!(held_by(root, "agent-a"), vec!["f.rs".to_string()]);
        assert!(release(root, "agent-a", "f.rs", false).is_ok());
    }
}
