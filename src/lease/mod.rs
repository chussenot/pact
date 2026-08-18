//! Advisory file leases: atomic lock files under `.pact/leases/`, with TTL,
//! steal, and re-entrant-refresh semantics. See docs/leases.md.

mod store;
mod types;

pub use store::*;
pub use types::*;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::events;
use crate::otel;
use crate::output::exit_with;
use crate::watch;

#[cfg(feature = "otel")]
use crate::repo::pact_dir;

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
fn worktree_stamp(repo_root: &Path) -> (Option<String>, Option<String>, Option<String>) {
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
fn holder_location(lease: &LeaseInfo) -> String {
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
fn warn_if_copy_diverged(
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

/// How long [`WriteGuard::acquire`] waits for the lock before giving up and
/// reporting contention rather than assuming anything about the current
/// holder. Generous relative to the critical section's normal cost (a
/// handful of filesystem syscalls) even under heavy N-way contention; see the
/// struct doc comment for why this is a diagnostic bound, not a reclaim
/// trigger.
const GUARD_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const GUARD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// Serializes every mutation to one lock path, closing the race
/// `verify_own_lease` could only narrow (pact-iup, pact-ehi).
///
/// `verify_own_lease` alone lets two racers each read `existing` as
/// takeover-eligible *before either writes*, then each write and each
/// independently see their own name on re-read: both get `Ok`. That is not a
/// hypothetical corner — reproduced directly against the compiled binary via
/// ordinary CLI-level `pact lease acquire` races (no fault injection, no
/// forced scheduling): double-wins in roughly 20-30% of rounds at N=6..10
/// concurrent racers on one pre-expired lock, and even 2 of 30 rounds at
/// plain N=2, plus genuine 3-way wins at N=6 and N=8. See
/// `antithesis/scratchbook/properties/lease-double-win-reachable.md` and
/// `n-way-worktree-double-win-scaling.md`. That evidence is what resolved the
/// deferral both beads stated it on: "implement iff a double-win is observed."
///
/// This guard makes the read of `existing` and the write that follows it one
/// atomic unit: the second racer, once it gets the guard, reads the FIRST
/// racer's fresh write as `existing` and makes its decision against current
/// reality — expired-reclaim, refresh and `--steal` all become correct
/// instead of merely less-wrong. `verify_own_lease` stays in place after each
/// write as a cheap check that the guard itself worked, not as the primary
/// defense anymore.
///
/// A real `flock(2)`, not a hand-rolled marker file — the first version of
/// this guard used a sibling `.guard` file created via `O_EXCL`, reclaimed
/// once it looked older than a fixed wall-clock threshold on the theory that
/// "older than the critical section could legitimately take" meant "the
/// holder crashed." TLA+ model checking (pact-kqb) proved that reasoning
/// unsound: under genuine N-way contention (the same N=5..10 shape
/// `lease-double-win-reachable.md` already reproduced), a live, working
/// holder can legitimately still be inside the critical section once the
/// threshold elapses — no crash required, just contention — and a waiter that
/// reclaims on that basis steals the guard out from under a holder who is
/// still using it, reopening the exact double-win this guard exists to
/// close, through a different door. A follow-up fix that only closed the
/// second flaw TLC found (`Drop` deleting the marker file unconditionally,
/// not just an ownership-checked ONE) was hand-verified NOT to help: in the
/// traced counterexample both racers complete their write and verify before
/// either `Drop` runs, so a token check on `Drop` never gets a chance to
/// matter. The staleness heuristic itself was the load-bearing flaw, and no
/// wall-clock heuristic can fix it — only genuine proof of the holder's death
/// can. `flock` provides that for free: the kernel releases it the instant
/// the holding process's file descriptor closes, on a clean exit AND on a
/// crash, so there is no "is the holder actually dead" question to answer
/// with a guess. The guard file itself is never deleted — `flock`'s
/// exclusivity is per-inode, so unlinking the path while a waiter might still
/// be about to open (and lock) that same name would let a new inode at the
/// same path start a fresh, unrelated lock series, splitting exactly the
/// mutual exclusion this exists to provide. An empty file left behind per
/// ever-contested lock path is the accepted cost.
struct WriteGuard {
    // Holds the fd open for the guard's lifetime; dropping it closes the fd,
    // which is what releases the flock. No explicit unlock call needed, and
    // nothing else about the file is inspected once held — only its exclusive
    // hold matters.
    _file: std::fs::File,
}

#[cfg(unix)]
mod flock_ffi {
    use std::os::unix::io::RawFd;

    // Not the `libc` crate: three constants and one syscall don't earn a new
    // dependency (this codebase already reaches for a bare `extern "C"` block
    // rather than pulling one in for less than this — see `otel.rs`'s urandom
    // read). `flock(2)` is POSIX; every Unix pact ships for links against it.
    extern "C" {
        fn flock(fd: RawFd, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    /// `Ok(true)`: acquired. `Ok(false)`: another process holds it right now
    /// (`EWOULDBLOCK`). `Err`: something else went wrong.
    pub fn try_lock_exclusive(fd: RawFd) -> std::io::Result<bool> {
        if unsafe { flock(fd, LOCK_EX | LOCK_NB) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            Ok(false)
        } else {
            Err(err)
        }
    }
}

/// `.pact/guards/<encoded>.lock.guard`, a SIBLING of `leases/`, never inside
/// it. The guard file is never deleted (see [`WriteGuard`]'s doc comment), so
/// anything that lists `.pact/leases/` expecting to see only `.lock` files —
/// `scan()`'s extension filter already handles that correctly, but a raw
/// directory count elsewhere does not — must never be handed one.
fn guard_file_path(lock_path: &Path) -> PathBuf {
    let leases_dir = lock_path.parent().unwrap_or(Path::new("."));
    let pact_dir = leases_dir.parent().unwrap_or(Path::new("."));
    let file_name = lock_path.file_name().unwrap_or_default();
    pact_dir
        .join("guards")
        .join(file_name)
        .with_extension("lock.guard")
}

impl WriteGuard {
    fn acquire(lock_path: &Path) -> Result<Self> {
        let path = guard_file_path(lock_path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // Never `create_new`: many processes opening this SAME file
        // concurrently is the normal case here — `flock`, not file creation,
        // is what provides exclusivity. Never removed on the way out either;
        // see the struct doc comment for why unlinking it would reopen the
        // exact race this whole guard exists to close.
        let file = std::fs::OpenOptions::new()
            .create(true)
            // Content is irrelevant — only the inode's identity as an flock
            // target matters — so never truncate an existing guard file, in
            // case a future version ever does put something in it.
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening guard {}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let deadline = std::time::Instant::now() + GUARD_WAIT_TIMEOUT;
            loop {
                if flock_ffi::try_lock_exclusive(file.as_raw_fd())
                    .with_context(|| format!("locking guard {}", path.display()))?
                {
                    return Ok(Self { _file: file });
                }
                if std::time::Instant::now() >= deadline {
                    // Reported, never silently seized: giving up here is the
                    // one thing that must NOT reclaim the guard on a guess —
                    // that guess is exactly what pact-kqb proved unsound.
                    return Err(exit_with(
                        2,
                        format!(
                            "lease guard on {} has been held by another process for over {}s; \
                             not proceeding — if it is truly stuck, that process needs to be \
                             found and stopped, not assumed dead",
                            lock_path.display(),
                            GUARD_WAIT_TIMEOUT.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(GUARD_POLL_INTERVAL);
            }
        }
        #[cfg(not(unix))]
        {
            // No Windows target, and the absence is a decision rather than an
            // omission (see release.yml) — `flock(2)` is the only sound
            // primitive here (see the struct doc comment for why a hand-rolled
            // marker file is not), and it is POSIX-only.
            compile_error!("WriteGuard requires flock(2); pact does not build on non-Unix");
        }
    }
}

/// After a `write_lease_atomic` that was meant to take ownership, re-read the
/// lock and confirm that it now belongs to `agent`. If another agent's
/// concurrent rename landed after ours, the file will name them instead — and
/// we must return exit 2 rather than falsely reporting that we hold the
/// lease.
///
/// Deliberately does NOT also compare `acquired_at` (pact-m7j.1.4): two
/// concurrent acquires under one `PACT_AGENT` value can each write a
/// different `acquired_at` for the same path, and whichever write lands
/// second makes the first's `acquired_at` stale on disk. That is a
/// same-identity refresh race, not a peer takeover — "the other one" is still
/// this agent, so the lease was never lost, and there is nobody to message.
/// Only a *different* on-disk agent means we actually lost the path.
///
/// Cost: one read. Applied on ALL post-conflict write paths: expired-takeover,
/// re-entrant refresh, `--steal`, and `renew`.
///
/// Once a live [`WriteGuard`] surrounds the read-decide-write sequence, this
/// can only ever succeed for the guard holder — nothing else can be mid-write
/// at the same time. Kept anyway as a cheap, independent check that the guard
/// mechanism itself worked, the same role an assertion plays after a lock.
fn verify_own_lease(lock_path: &Path, agent: &str) -> Result<()> {
    let on_disk = read_lease(lock_path)?;
    if on_disk.agent == agent {
        return Ok(());
    }
    Err(exit_with(
        2,
        format!(
            "lease on {} was taken by {} in a concurrent steal; this agent did not win",
            lock_path.display(),
            on_disk.agent
        ),
    ))
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
fn log_event(
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
fn count_transition(outcome: &'static str) {
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
fn record_hold(lease: &LeaseInfo, outcome: &'static str) {
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
fn mark_conflict(repo_root: &Path, agent: &str, relative: &str, blocker: &str) {
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
fn record_wait(repo_root: &Path, agent: &str, relative: &str) {
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
fn sweep_wait_markers(repo_root: &Path, agent: &str) {
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
fn mark_conflict(_repo_root: &Path, _agent: &str, _relative: &str, _blocker: &str) {}
#[cfg(not(feature = "otel"))]
#[inline(always)]
fn record_wait(_repo_root: &Path, _agent: &str, _relative: &str) {}
#[cfg(not(feature = "otel"))]
#[inline(always)]
fn sweep_wait_markers(_repo_root: &Path, _agent: &str) {}

pub fn acquire(
    repo_root: &Path,
    agent: &str,
    path: &str,
    ttl_secs: u64,
    steal: bool,
    note: Option<String>,
) -> Result<AcquireOutcome> {
    current_store().acquire(repo_root, agent, path, ttl_secs, steal, note)
}

/// Telemetry wrapper around [`acquire_inner`]. Separate so the span covers the
/// whole operation — including the post-rename verify — and so the four
/// success branches inside do not each have to remember to close the wait
/// histogram.
fn acquire_fs(
    repo_root: &Path,
    agent: &str,
    path: &str,
    ttl_secs: u64,
    steal: bool,
    note: Option<String>,
) -> Result<AcquireOutcome> {
    // Validated HERE and nowhere else: `acquire_many_fs` reaches every path through
    // `acquire`, which lands here, so this runs exactly once per path in both the
    // single- and multi-path cases. `acquire_inner` normalizes again for its own use and
    // must not re-warn.
    let relative = resolve_claimable(repo_root, path)?;
    let mut sp = otel::span("pact.lease.acquire");
    sp.set("pact.path", relative.clone());
    sp.set("pact.lease.ttl_secs", ttl_secs);

    let outcome = acquire_inner(repo_root, agent, path, ttl_secs, steal, note);
    match &outcome {
        Ok(o) => {
            sp.set("pact.lease.stolen", o.stolen);
            record_wait(repo_root, agent, &relative);
        }
        // `held` and not the error text: a span status is a bounded reason
        // code, and the message names another agent.
        Err(e) if crate::output::code_for(e) == 2 => {
            // The holder goes on the span as its own attribute. It is
            // deliberately NOT a metric dimension — an agent name is unbounded
            // and would mint a series per fleet member — but the span is
            // exactly where an unbounded value belongs, and without it
            // "click through, don't group by" clicks through to nothing.
            // That was pact-ebe: a refused acquire named the victim and never
            // the holder, so who-blocks-whom lived only in `pact log`.
            // One lock read, on a failure path only.
            if let Ok(held) = lock_file_path(repo_root, &relative).and_then(|p| read_lease(&p)) {
                sp.set("pact.lease.holder", held.agent);
            }
            sp.fail("held");
        }
        Err(_) => sp.fail("error"),
    }
    outcome
}

fn acquire_inner(
    repo_root: &Path,
    agent: &str,
    path: &str,
    ttl_secs: u64,
    steal: bool,
    note: Option<String>,
) -> Result<AcquireOutcome> {
    let relative = normalize_path(repo_root, path);
    let lock_path = lock_file_path(repo_root, &relative)?;
    // pact-m7j.4.4/4.5: the clock-corrected "now", not raw `Utc::now()` — see
    // `effective_now`'s doc comment.
    let now = effective_now(repo_root);
    let (branch, worktree, invoked_from) = worktree_stamp(repo_root);
    // Hashed BEFORE the claim lands, so it describes the content the holder is
    // taking responsibility for rather than whatever a racing writer left
    // after. Best-effort: `hash_objects` yields nothing for a path that does
    // not exist (leasing a file you are about to create is documented), and a
    // lease must never fail because a diff could not be prepared.
    let content_hash = crate::git_history::hash_objects(repo_root, std::slice::from_ref(&relative))
        .remove(&relative);
    // Mutable so the expired-reclaim and --steal branches below can copy
    // `existing.extra` onto it before writing (pact-m7j.9.8) — everything
    // else about this lease (agent, note, branch/worktree) is already
    // deliberately fresh, never inherited, so `extra` starts the same way and
    // only the two takeover branches override it.
    let mut new_lease = LeaseInfo {
        agent: agent.to_string(),
        path: relative.clone(),
        acquired_at: now.to_rfc3339(),
        ttl_secs,
        note,
        branch,
        worktree,
        // Recorded HERE, at acquire, because the expiry that may eventually close this
        // lease is written by a different process in a different place after the holder
        // has gone (pact-83r.3 / finding 5a). Absent in a repo with no worktrees — see
        // `worktree_stamp`.
        invoked_from,
        content_hash,
        extra: BTreeMap::new(),
    };

    // Claim the path with a LINK, not with create_new + write.
    //
    // create_new() gave exclusivity and nothing else: the file existed, empty,
    // between the open and the write. Every other write path in this module
    // goes through write_lease_atomic; this one did not, because O_EXCL already
    // guarantees only one winner — but exclusivity is not atomicity of content.
    // A reader landing in that window got `EOF while parsing a value`, and
    // `pact doctor` called it "1 unreadable lock file (remove manually from
    // .pact/leases/)" — advice that, followed during the window, deletes a live
    // agent's lock. Agent Mail met the same thing: "concurrent agents could read
    // partially-written lease JSON" (d8d1cc7), and in the same commit stopped
    // treating absent metadata as proof the owner was dead, because there is a
    // window between claiming a lock and describing it.
    //
    // hard_link is the primitive that does both jobs: it is atomic, and it fails
    // with AlreadyExists if the destination is taken. So the name appears only
    // once the bytes behind it are complete, and only one caller can create it.
    // Both files live in .pact/leases/, so they are always on one filesystem.
    let json = serde_json::to_string_pretty(&new_lease)?;
    let staged = temp_sibling(&lock_path);
    std::fs::write(&staged, &json).with_context(|| format!("writing {}", staged.display()))?;
    let claimed = std::fs::hard_link(&staged, &lock_path);
    // The staging file has served its purpose either way; the lock now has its
    // own link to the same inode.
    let _ = std::fs::remove_file(&staged);

    match claimed {
        Ok(()) => {
            // pact-m7j.9.1: an empty `.pact/leases/` — a fresh clone, or the
            // very recovery `pact doctor` prescribes for a corrupt lock —
            // looks identical, locally, to a path nobody has ever touched.
            // But the SHARED `events.jsonl` might still show a prior
            // "acquired" for this path with no later released/expired/
            // stolen/force-released row: a claim nothing ever closed out.
            // Bounded to events.jsonl's own line cap, not `audit::reconstruct`'s
            // full-log walk, which answers a broader question at a much higher
            // cost. A warning, not a refusal — a doctor-prescribed manual
            // `leases/` wipe produces this exact shape and is a legitimate
            // recovery, so blocking here would misfire on precisely the case
            // doctor tells people to run.
            //
            // ONE parse for both warnings (pact-hxy). These used to be
            // `events::owner_of` followed by a second, separate lookup that read
            // the same bytes twice — measured at 2.6 ms apiece against a claim
            // that is otherwise microseconds.
            let facts = events::acquire_facts(repo_root, &relative).unwrap_or_default();
            if let Some(prior) = &facts.owner {
                if prior.kind == "acquired" {
                    crate::output::warn(&format!(
                        "warning: the shared event log's last word on {relative} is an \
                         unresolved acquire by {} at {} (no later release/expiry/steal on \
                         record) — .pact/leases/ has no matching lock locally, so this acquire \
                         is proceeding, but that prior claim was never closed out",
                        prior.agent, prior.at
                    ));
                }
            }
            warn_if_copy_diverged(
                &relative,
                new_lease.content_hash.as_deref(),
                facts.left_behind.as_ref(),
            );
            log_event(
                repo_root,
                agent,
                "acquired",
                &relative,
                new_lease.note.clone(),
                new_lease.ttl_secs,
                new_lease.content_hash.clone(),
                None,
            );
            count_transition("acquired");
            Ok(AcquireOutcome {
                lease: new_lease,
                stolen: false,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Serializes this whole branch — see WriteGuard's doc comment.
            // Held from before the read that decides "is this takeover-eligible"
            // through the write that acts on that decision, so a second racer
            // can never read the same pre-mutation `existing` we did.
            let _guard = WriteGuard::acquire(&lock_path)?;
            let existing = match read_lease(&lock_path) {
                Ok(existing) => existing,
                // A lock file whose JSON cannot be parsed used to fail EVERY
                // acquire attempt here, `--steal` included — even though
                // overriding a problematic existing claim is the entire
                // reason `--steal` exists. Ownership cannot be determined
                // from unparsable content, so treat it the same as an
                // expired lease: reclaimable, but only under `--steal`,
                // which is the caller explicitly asking to override
                // whatever is there.
                Err(read_err) if steal => {
                    crate::output::warn(&format!(
                        "warning: lock file for {relative} is corrupt ({read_err:#}); \
                         recovering it via --steal"
                    ));
                    write_lease_atomic(&lock_path, &new_lease)?;
                    verify_own_lease(&lock_path, agent)?;
                    log_event(
                        repo_root,
                        agent,
                        "stolen",
                        &relative,
                        Some(format!(
                            "recovered a corrupt lock file via --steal ({read_err:#})"
                        )),
                        new_lease.ttl_secs,
                        new_lease.content_hash.clone(),
                        None,
                    );
                    count_transition("stolen");
                    return Ok(AcquireOutcome {
                        lease: new_lease,
                        stolen: true,
                    });
                }
                // Same "ownership unknown" case as release_fs's corrupt-lock
                // branch, so it gets the same exit code: 2, not the generic 1
                // this used to fall through to by returning the raw parse
                // error unwrapped. AGENTS.md tells every agent to branch on
                // exit 2 for "this path is not available", not on message
                // text — a corrupt lock is exactly that (pact-m7j.4.8).
                Err(read_err) => {
                    return Err(exit_with(
                        2,
                        format!(
                            "lock file for {relative} is corrupt ({read_err:#}); ownership \
                             cannot be determined — use --steal to recover it"
                        ),
                    ));
                }
            };

            if is_expired(&existing, now) {
                // A takeover, not a fresh claim: whatever a newer/older binary
                // stamped on the dead holder's lease survives the reclaim
                // (pact-m7j.9.8), unlike the re-entrant-refresh branch below,
                // which deliberately starts every field fresh.
                new_lease.extra = existing.extra.clone();
                write_lease_atomic(&lock_path, &new_lease)?;
                verify_own_lease(&lock_path, agent)?;
                // The previous claim ended here, and this is the only moment
                // anyone notices (pact-rnc.13). Without this row the feed's last
                // word on `existing.agent` is still "acquired", i.e. it reports a
                // dead agent as the current holder. It also tells a consumer
                // grouping by `kind` which "stolen" rows are routine reclaims: a
                // reclaim is always preceded by an "expired" row for the same
                // path, a `--steal` override never is.
                log_event(
                    repo_root,
                    &existing.agent,
                    "expired",
                    &relative,
                    Some(format!(
                        "lease lapsed (ttl {}s), taken over by {agent}",
                        existing.ttl_secs
                    )),
                    // The DEAD holder's ttl: this row closes their window.
                    existing.ttl_secs,
                    None,
                    None,
                );
                // Reported as `stolen` by AcquireOutcome, so logged as "stolen"
                // too — but the detail says *why*, because taking over a dead
                // claim is not the same act as overriding a live one.
                log_event(
                    repo_root,
                    agent,
                    "stolen",
                    &relative,
                    Some(format!(
                        "took over expired lease of {} (ttl {}s)",
                        existing.agent, existing.ttl_secs
                    )),
                    // The NEW holder's ttl: this row opens their window.
                    new_lease.ttl_secs,
                    new_lease.content_hash.clone(),
                    None,
                );
                count_transition("expired");
                record_hold(&existing, "expired");
                count_transition("reclaimed");
                Ok(AcquireOutcome {
                    lease: new_lease,
                    stolen: true,
                })
            } else if existing.agent == agent {
                // Re-entrant refresh: same holder, just bump acquired_at.
                //
                // `existing.agent == agent` is a plain string comparison against
                // `PACT_AGENT` (or `--agent`), which `identity::validate` checks
                // for FORMAT only, never provenance — pact has no PID, session,
                // or credential to compare, and every real caller of `acquire`
                // is a fresh CLI process per invocation (no long-lived caller
                // re-acquires within one process; see acceptance discussion on
                // pact-m7j.7.1), so a PID/session field would only ever
                // misfire on the ordinary "run `acquire` again to refresh"
                // workflow rather than catch anything. Investigated and
                // rejected — see docs/leases.md's trust-boundary section.
                //
                // This branch is therefore silent by design (no warning, no
                // `stolen` flag) unlike `--steal`, which knows it is
                // overriding a *different* agent. The one thing that DOES
                // distinguish it from a first-time `acquire`: `log_event`
                // below writes kind `"renewed"`, not `"acquired"`, so
                // `pact log` / `.pact/events.jsonl` / `pact audit` already
                // record every refresh distinctly — that is the auditability
                // this branch has, and it existed before this comment did.
                write_lease_atomic(&lock_path, &new_lease)?;
                verify_own_lease(&lock_path, agent)?;
                log_event(
                    repo_root,
                    agent,
                    "renewed",
                    &relative,
                    new_lease.note.clone(),
                    new_lease.ttl_secs,
                    None,
                    None,
                );
                count_transition("renewed");
                Ok(AcquireOutcome {
                    lease: new_lease,
                    stolen: false,
                })
            } else if steal {
                crate::output::warn(&format!(
                    "warning: stealing non-expired lease on {relative} held by {} (advisory override via --steal)",
                    holder_location(&existing)
                ));
                // Same takeover reasoning as the expired-reclaim branch above:
                // preserve the displaced lease's unknown fields (pact-m7j.9.8).
                new_lease.extra = existing.extra.clone();
                let (_, displaced_remaining) = age_and_remaining(&existing, now);
                write_lease_atomic(&lock_path, &new_lease)?;
                verify_own_lease(&lock_path, agent)?;
                log_event(
                    repo_root,
                    agent,
                    "stolen",
                    &relative,
                    Some(format!(
                        "displaced live holder {} via --steal",
                        existing.agent
                    )),
                    new_lease.ttl_secs,
                    new_lease.content_hash.clone(),
                    None,
                );
                // Closes the displaced holder's window, as the "expired" row does
                // one branch up and for the identical reason: without it the
                // feed's last word on `existing.agent` is still "acquired", so
                // every consumer keeps naming a holder who was overridden minutes
                // ago as the current one, and `audit --check double-win` reads
                // every LATER acquire of this path as an overlap against them —
                // nine such reports in the crucible run, eight of them naming one
                // SIGKILLed agent, none of them a real concurrent hold
                // (pact-mqw.1).
                //
                // Logged AFTER the "stolen" row, and that order is load-bearing.
                // A --steal over a live claim IS a genuine overlap at the instant
                // it happens, and reporting it is deliberate
                // (`stealing_a_live_lease_is_a_double_win`). Closing first would
                // silently retire that detection; closing second keeps it and
                // stops the window leaking past the steal. Each branch's order
                // mirrors what actually happened: a reclaim's holder was already
                // gone before the takeover, a steal's holder was still live
                // during it.
                //
                // A distinct kind rather than reusing "expired", because the two
                // are not the same event and the difference is the point:
                // "expired" means a TTL lapsed and nobody was harmed,
                // "displaced" means a live claim was overridden. Keeping them
                // apart lets a consumer grouping by `kind` tell a routine reclaim
                // from a forced override without parsing prose.
                log_event(
                    repo_root,
                    &existing.agent,
                    "displaced",
                    &relative,
                    Some(format!(
                        "live lease overridden by {agent} via --steal ({} still remaining of ttl {}s)",
                        human_secs(displaced_remaining),
                        existing.ttl_secs
                    )),
                    // The DISPLACED holder's ttl: this row closes their window.
                    existing.ttl_secs,
                    None,
                None,
            );
                count_transition("displaced");
                count_transition("stolen");
                record_hold(&existing, "stolen");
                Ok(AcquireOutcome {
                    lease: new_lease,
                    stolen: true,
                })
            } else {
                let (age, remaining) = age_and_remaining(&existing, now);
                count_transition("conflicted");
                // Leave the breadcrumb *before* returning: the acquire that
                // finally succeeds is a different process and this is the only
                // record that the agent was ever locked out.
                mark_conflict(repo_root, agent, &relative, &existing.agent);
                // pact-juz.1: `.pact/waits/` markers are excluded from `pact
                // audit`'s history by design (telemetry, not history — see
                // docs/audit.md), and the OTEL counter above is compiled out
                // entirely without --features otel. Before this, a denied
                // acquire left NOTHING in .pact/events.jsonl: reproduced live
                // on a real 15-agent build where paths with 6-8 distinct
                // holders showed zero contention in `pact log`/`pact audit`,
                // because there was nothing to show regardless of whether any
                // real refusal ever happened. Logged under the REQUESTER (the
                // one refused), matching every other kind's convention that
                // `agent` is whose row this is; the holder's identity and
                // remaining TTL go in `detail`. Neither an open nor a close in
                // `audit::reconstruct` — same neutral shape as "renewed"/
                // "restored" — so existing hold-duration and double-win math
                // is unaffected by this kind existing in a log.
                let note_suffix = new_lease
                    .note
                    .as_deref()
                    .map(|n| format!(" — my note: {n}"))
                    .unwrap_or_default();
                // Not `log_event`: the holder's four facts have no parameter
                // there, and they are the whole point of pact-1gv.1 — every one
                // of them was already in the prose below and reachable only by
                // regex. Composed from the SAME `existing`/`remaining` this
                // message reads, in one place, so the two representations cannot
                // disagree.
                //
                // `ttl_secs` stays the REFUSED agent's requested ttl, which is
                // what it has always been. It is the field a reader mistakes for
                // the holder's remaining time; `holder_remaining_secs` is the one
                // that answers that, and the two differ by a factor of 24 in the
                // run this came from.
                events::append(
                    repo_root,
                    &events::Event {
                        at: Utc::now().to_rfc3339(),
                        agent: agent.to_string(),
                        kind: "refused".to_string(),
                        path: Some(relative.clone()),
                        detail: Some(format!(
                            "held by {} ({age}s old, {remaining}s remaining), use --steal to \
                             override{note_suffix}",
                            holder_location(&existing)
                        )),
                        ttl_secs: Some(ttl_secs),
                        covers_lines: None,
                        actor: None,
                        displaced: None,
                        // append() computes the real value; see Event::chain_hash.
                        chain_hash: None,
                        // Likewise stamped by append().
                        invoked_from: None,
                        collected_from: None,
                        scope: None,
                        pact_version: None,
                        content_hash: None,
                        subscriber: None,
                        message_id: None,
                        protocol_hash: None,
                        head: None,
                        holder: Some(existing.agent.clone()),
                        holder_remaining_secs: Some(remaining),
                        holder_branch: existing.branch.clone(),
                        holder_worktree: existing.worktree.clone(),
                        ..Default::default()
                    },
                );
                // The one thing a refused agent most needs and was never told:
                // whether it has already arranged to be notified when this path
                // comes free (pact-1gv.2). Printed BEFORE the error, because the
                // error is what a reader stops at.
                //
                // Advisory and best-effort — an unreadable registry says nothing
                // rather than guessing, and neither branch can change the exit
                // code, which stays 2 as the protocol contract requires.
                if watch::is_subscribed(repo_root, agent, &relative) {
                    crate::output::warn(&format!(
                        "note: you already watch {relative} — pact will send you the diff when \
                         {} releases it ({}s left on their lease). Pick up other ready work; do \
                         NOT poll for this path",
                        existing.agent, remaining
                    ));
                } else {
                    crate::output::warn(&format!(
                        "note: `pact watch add {relative}` and pact will tell you when {} \
                         releases it, instead of you asking again",
                        existing.agent
                    ));
                }
                // The holder's LOCATION, not just their name. A peer in another
                // worktree is editing a checkout this reader cannot see, so
                // "held by agent-a" alone invites them to inspect their own copy,
                // find it untouched, and conclude the lease is stale.
                Err(exit_with(
                    2,
                    format!(
                        "lease on {relative} is held by {} ({age}s old, {remaining}s remaining); use --steal to override",
                        holder_location(&existing)
                    ),
                ))
            }
        }
        Err(e) => Err(e).with_context(|| format!("creating lock file {}", lock_path.display())),
    }
}

/// `agent`'s own live lease on `relative`, if it has one — the whole lease, not
/// just "yes": an [`acquire_many`] rollback has to put back exactly what it
/// found, TTL and note included, not merely leave *a* lease behind.
fn held_by_self(repo_root: &Path, agent: &str, relative: &str) -> Option<LeaseInfo> {
    let lock_path = lock_file_path(repo_root, relative).ok()?;
    let existing = read_lease(&lock_path).ok()?;
    (existing.agent == agent && !is_expired(&existing, effective_now(repo_root)))
        .then_some(existing)
}

/// Acquire several paths atomically: either the agent ends up holding all of
/// them, or it holds exactly what it held before the call (pact-rnc.21).
///
/// Why all-or-nothing: an agent that owns a new module also needs the one line
/// in `main.rs` that declares it. Claiming them one at a time and failing
/// halfway leaves it holding claims it must remember to unwind — the kind of
/// bookkeeping that gets forgotten and shows up later as a stale lock nobody
/// owns. On failure this returns the *first* unavailable path's error, so the
/// message the agent sees names the path it actually has to negotiate over.
///
/// Rollback releases what this call took and *restores* what it merely
/// refreshed. A path the agent already held survives the rollback with the lease
/// it walked in with: `acquire` on a pre-held path is a re-entrant refresh, so it
/// has already overwritten that lease's `acquired_at`, TTL and note by the time
/// a later path in the batch turns out to be unavailable. Leaving the refresh in
/// place is how a call that reports "nothing was taken" silently downgraded a
/// live 900s claim with a note to a 30s claim without one, after which peers saw
/// it as reclaimable and stole a file the agent was still editing (pact-rnc.21).
pub fn acquire_many(
    repo_root: &Path,
    agent: &str,
    paths: &[String],
    ttl_secs: u64,
    steal: bool,
    note: Option<String>,
) -> Result<Vec<AcquireOutcome>> {
    current_store().acquire_many(repo_root, agent, paths, ttl_secs, steal, note)
}

fn acquire_many_fs(
    repo_root: &Path,
    agent: &str,
    paths: &[String],
    ttl_secs: u64,
    steal: bool,
    note: Option<String>,
) -> Result<Vec<AcquireOutcome>> {
    let mut outcomes: Vec<AcquireOutcome> = Vec::new();
    let mut newly_taken: Vec<String> = Vec::new();
    let mut refreshed: Vec<LeaseInfo> = Vec::new();

    for path in paths {
        let relative = normalize_path(repo_root, path);
        let pre_held = held_by_self(repo_root, agent, &relative);
        match acquire(repo_root, agent, path, ttl_secs, steal, note.clone()) {
            Ok(outcome) => {
                match pre_held {
                    Some(before) => refreshed.push(before),
                    None => newly_taken.push(outcome.lease.path.clone()),
                }
                outcomes.push(outcome);
            }
            Err(e) => {
                // Best-effort unwind: a rollback failure must not mask the
                // conflict that caused it, which is what the caller has to act
                // on. Any lock we cannot remove expires on its own TTL.
                for taken in &newly_taken {
                    let _ = release(repo_root, agent, taken, false);
                }
                // Put the pre-held leases back byte for byte. The "renewed"
                // event the refresh already logged stays in the feed — but
                // logged alone it would be the feed's last word on this path,
                // telling a reader (and `pact audit --check stale-holds`,
                // pact-m7j.1.3) that a real renewal justified the hold when it
                // was undone moments later. The "restored" event below closes
                // that gap (pact-m7j.1.2).
                //
                // Guarded and re-checked, not unconditional (pact-m7j.1.1): the
                // gap between the batch's refresh and this rollback is exactly
                // when a peer could legitimately `--steal` the still-refreshed
                // lease. Restoring "before" over their fresh claim would
                // silently destroy it; only restore if we are still the agent
                // of record.
                for before in &refreshed {
                    let Ok(lock_path) = lock_file_path(repo_root, &before.path) else {
                        continue;
                    };
                    let Ok(_guard) = WriteGuard::acquire(&lock_path) else {
                        continue;
                    };
                    let still_ours = read_lease(&lock_path)
                        .map(|l| l.agent == agent)
                        .unwrap_or(false);
                    if still_ours {
                        let _ = write_lease_atomic(&lock_path, before);
                        log_event(
                            repo_root,
                            agent,
                            "restored",
                            &before.path,
                            Some(format!(
                                "batch acquire failed on a later path; reverted this refresh, \
                                 restoring the pre-batch lease (acquired_at {})",
                                before.acquired_at
                            )),
                            // The ttl now back in force: the pre-batch one, not
                            // the batch's, so a stale-holds check judges the
                            // restored hold by the promise that is actually
                            // live again.
                            before.ttl_secs,
                            None,
                            None,
                        );
                    }
                }
                // One per batch that actually unwound, not one per path, and
                // not at all when there was nothing to give back. `main.rs`
                // routes even a one-path `lease acquire` through here, so
                // counting every failure as a rollback made an ordinary
                // conflict — the commonest event in a fleet — report a
                // rollback that never happened. The paths it did give back
                // were each counted `acquired` and then `released`, so those
                // two stay balanced; this is the counter that says the pair
                // was churn rather than work.
                if !newly_taken.is_empty() || !refreshed.is_empty() {
                    count_transition("rolled_back");
                }
                return Err(e);
            }
        }
    }

    Ok(outcomes)
}

/// Release a lease. See [`ReleaseOutcome`] for why the four cases are told apart
/// rather than all reported as success.
pub fn release(repo_root: &Path, agent: &str, path: &str, force: bool) -> Result<ReleaseOutcome> {
    current_store().release(repo_root, agent, path, force)
}

fn release_fs(repo_root: &Path, agent: &str, path: &str, force: bool) -> Result<ReleaseOutcome> {
    release_relative(repo_root, agent, &normalize_path(repo_root, path), force)
}

/// [`release_fs`] for a path that is ALREADY repo-relative.
///
/// The split exists because `normalize_path` resolves a relative path against the
/// process CWD, so re-normalizing a path that came out of the lease store mangles it
/// from any directory but the repo root — `a.rs` read from `.pact/leases/` becomes
/// `sub/a.rs` when the agent happens to be standing in `sub/`, which is a lock that
/// does not exist.
///
/// That was finding 2 of the fleet's field audit, and the worst bug pact has shipped:
/// `release --all` reported "held no leases" while `lease ls` showed the leases in the
/// same second, so agents ended their turn believing they had released everything and
/// left live locks behind for 45 minutes of TTL. One agent had to `--steal` from a peer
/// that had already finished — the single case the protocol reserves for "when you know
/// a peer is gone". It also silently corrupted every contention metric: a leaked lease
/// later stolen reads as contention that never happened.
///
/// Same class of defect as `msg::send` re-normalizing an `about` path `run_msg` had
/// already canonicalized. One normalization, at the boundary, and never again.
fn release_relative(
    repo_root: &Path,
    agent: &str,
    relative: &str,
    force: bool,
) -> Result<ReleaseOutcome> {
    let relative = relative.to_string();
    let lock_path = lock_file_path(repo_root, &relative)?;
    // The clock-corrected now, same as acquire's — see `effective_now`.
    let now = effective_now(repo_root);
    let mut sp = otel::span("pact.lease.release");
    sp.set("pact.path", relative.clone());

    if !lock_path.exists() {
        // pact-m7j.9.6: a miss here is indistinguishable from "already
        // released" — but it is also exactly what a scope/topology change
        // since acquire time looks like, and that case has a real lock
        // sitting unreleased in the OTHER directory. Warn rather than stay
        // silent; still `Ok(None)`, because a plain miss really is
        // idempotent and this is advisory, not a reason to fail the call.
        if let Some(other) = other_candidate_lock_path(repo_root, &relative) {
            if other.exists() {
                crate::output::warn(&format!(
                    "warning: no lease found at {}, but one exists at {}; did \
                     PACT_WORKTREE_SCOPE or this repository's worktree topology \
                     change since this lease was acquired?",
                    lock_path.display(),
                    other.display()
                ));
            }
        }
        // Idempotent either way, but not the same news. A lock that lapsed and
        // was collected leaves nothing on disk, so the filesystem cannot tell
        // "already released" from "your TTL ran out under you" — the log can,
        // and only this agent's own row answers it (see
        // `events::last_custody_by`).
        return Ok(match events::last_custody_by(repo_root, &relative, agent) {
            Ok(Some(own)) if own.kind == "expired" => {
                let since_secs = chrono::DateTime::parse_from_rfc3339(&own.at)
                    .ok()
                    .map(|at| (effective_now(repo_root) - at.with_timezone(&Utc)).num_seconds());
                ReleaseOutcome::AlreadyExpired {
                    at: own.at,
                    ttl_secs: None,
                    since_secs,
                }
            }
            _ => ReleaseOutcome::NothingHeld,
        });
    }
    // release_fs had no verify guard at all (pact-m7j.1.5): without it, a
    // concurrent legitimate takeover's write could land between this read and
    // the delete below, and we would delete their fresh claim believing it was
    // still ours. Same guard as acquire_inner's takeover branches.
    let _guard = WriteGuard::acquire(&lock_path)?;
    let existing = match read_lease(&lock_path) {
        Ok(lease) => lease,
        // Released by a peer while we waited for the guard.
        Err(_) if !lock_path.exists() => return Ok(ReleaseOutcome::NothingHeld),
        // Corrupt content means ownership cannot be checked (`existing.agent
        // == agent` below has no `existing.agent` to compare), so a plain
        // release must refuse rather than guess. `--force` is the same
        // override lever every other conflict in this function already uses,
        // not a new concept (pact-m7j.4.2).
        Err(e) if !force => {
            count_transition("conflicted");
            sp.fail("corrupt");
            return Err(exit_with(
                2,
                format!(
                    "lock file for {relative} is corrupt and its holder cannot be verified \
                     ({e:#}); use --force to remove it"
                ),
            ));
        }
        Err(e) => {
            crate::output::warn(&format!(
                "warning: force-removing corrupt lock file for {relative} ({e:#}); its holder \
                 could not be verified, so no one is named as displaced"
            ));
            match std::fs::remove_file(&lock_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("removing {}", lock_path.display()));
                }
            }
            // Not `log_event`: its `ttl_secs: u64` would have to invent a
            // number for a lease whose real TTL was never readable, and
            // `Event::ttl_secs` exists specifically so "unknown" can be
            // represented as `None` rather than a fabricated value (see its
            // doc comment). `--force` removed *something*, but no agent name
            // survives to report as displaced.
            events::append(
                repo_root,
                &events::Event {
                    at: Utc::now().to_rfc3339(),
                    agent: agent.to_string(),
                    kind: "force-released".to_string(),
                    path: Some(relative.clone()),
                    detail: Some(format!("removed a corrupt lock file via --force ({e:#})")),
                    ttl_secs: None,
                    covers_lines: None,
                    actor: None,
                    // No agent name survived to report as displaced (see the
                    // comment above): correctly orphaned, not a bug this
                    // field is meant to fix (pact-m7j.2.6).
                    displaced: None,
                    // append() computes the real value; see
                    // Event::chain_hash (pact-m7j.2.5).
                    chain_hash: None,
                    // Likewise stamped by append(); see Event::invoked_from
                    // (pact-ler.1).
                    invoked_from: None,
                    collected_from: None,
                    scope: None,
                    pact_version: None,
                    content_hash: None,
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
            count_transition("force_released");
            // Nothing survived to name as displaced, so this is not
            // `ForceReleased` — but a lock did go away, which is what
            // `removed_a_lock` reports.
            return Ok(ReleaseOutcome::Released {
                past_ttl_secs: None,
            });
        }
    };

    let displaced = if existing.agent == agent {
        None
    } else if force {
        Some(existing.agent.clone())
    } else {
        count_transition("conflicted");
        // The holder goes on the SPAN, never on the metric. That split is the
        // whole cardinality argument above, and it only works if the span
        // actually carries the name — otherwise "click through, don't group by"
        // means clicking through to nothing, which is what pact-ebe found: a
        // refused acquire named the victim and not the holder, so the
        // who-blocks-whom edge existed in `pact log` and nowhere else.
        sp.set("pact.lease.holder", existing.agent.clone());
        sp.fail("held");
        return Err(exit_with(
            2,
            format!(
                "lease on {relative} is held by {}, not {agent} (use --force to override)",
                existing.agent
            ),
        ));
    };

    match std::fs::remove_file(&lock_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", lock_path.display())),
    }

    match &displaced {
        Some(holder) => {
            // Not `log_event`: `audit::reconstruct` needs the displaced
            // holder's own name to close THEIR window (pact-m7j.2.6) — this
            // event's own `agent` is the one who forced it, same asymmetry
            // as `expired`'s doc comment describes for the opposite case —
            // and `log_event` has no parameter for a second identity.
            events::append(
                repo_root,
                &events::Event {
                    at: Utc::now().to_rfc3339(),
                    agent: agent.to_string(),
                    kind: "force-released".to_string(),
                    path: Some(relative.clone()),
                    detail: Some(format!("destroyed live claim of {holder}")),
                    ttl_secs: Some(existing.ttl_secs),
                    covers_lines: None,
                    actor: None,
                    displaced: Some(holder.clone()),
                    // append() computes the real value; see
                    // Event::chain_hash (pact-m7j.2.5).
                    chain_hash: None,
                    // Likewise stamped by append(); see Event::invoked_from
                    // (pact-ler.1).
                    invoked_from: None,
                    collected_from: None,
                    scope: None,
                    pact_version: None,
                    // Same as the plain-release branch: what the displaced
                    // holder's copy contained at the moment it was taken away,
                    // so a later acquirer can still tell whether its own copy
                    // diverged (pact-mqw.3).
                    content_hash: crate::git_history::hash_objects(
                        repo_root,
                        std::slice::from_ref(&relative),
                    )
                    .remove(&relative),
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
            count_transition("force_released");
            record_hold(&existing, "force_released");
        }
        None => {
            record_hold(&existing, "released");
            log_event(
                repo_root,
                agent,
                "released",
                &relative,
                existing.note.clone(),
                existing.ttl_secs,
                // The content this holder LEFT, where the acquire event records
                // the content they took responsibility for. Without it a lease is
                // exclusive in time and invisible across worktrees: the next
                // acquirer has nothing to compare its own copy against
                // (pact-mqw.3). Best-effort, like every other hash here — a
                // release must never fail because git could not be asked.
                crate::git_history::hash_objects(repo_root, std::slice::from_ref(&relative))
                    .remove(&relative),
                None,
            );
            count_transition("released");
        }
    }

    // pact-8qu: the release has already landed — lock removed, event written,
    // metric counted — before a single subscriber is looked up. That ordering
    // is the guarantee: `notify_release` is infallible by signature and cannot
    // reach anything above it, so no notification failure can leave a lease
    // held, change this function's return value, or alter its exit code.
    //
    // Both the plain and the force-released branches deliver. Expiry does NOT
    // (see `collect_expired`): a lapsed lease means nobody is present to have
    // changed anything deliberately, and the at-acquire content is as likely
    // to differ because a peer edited the file as because the dead holder did.
    watch::notify_release(
        repo_root,
        &existing.agent,
        &relative,
        existing.content_hash.as_deref(),
    );
    Ok(match displaced {
        Some(displaced) => ReleaseOutcome::ForceReleased { displaced },
        None => ReleaseOutcome::Released {
            // Measured against the plain TTL, not ttl+GRACE_SECS: the grace
            // window is pact's tolerance for a skewed clock before it lets a
            // PEER reclaim, not an extension of the promise this agent made.
            // The lock was still here, so nobody reclaimed — but the path was
            // unprotected for this long and the holder should have renewed.
            past_ttl_secs: {
                let over = age_and_remaining(&existing, now).1;
                (over < 0).then_some(-over)
            },
        },
    })
}

/// Unlink an expired lock file and record the lapse as an `"expired"` event
/// (pact-rnc.13).
///
/// The event is the point, not the unlink. A lease that ends by expiry used to
/// leave the feed's last word on its holder as `"acquired"`, so the one moment
/// `pact log` is most needed — an agent died holding a file, its TTL lapsed,
/// someone ran `lease ls` and collected the lock — was the moment it lied,
/// naming a dead agent as the current holder of a file whose lock is already
/// gone. A permanently false trace is worse than the missing trace the bead was
/// filed for.
///
/// `agent` is the holder whose lease lapsed, never whoever happened to run the
/// command that collected it. Logged only by the process that actually won the
/// unlink, so two concurrent `lease ls` runs cannot report one lapse twice.
fn collect_expired(repo_root: &Path, lock_path: &Path, lease: &LeaseInfo) {
    if std::fs::remove_file(lock_path).is_ok() {
        // Only the process that won the unlink counts it, for the same reason
        // it is the only one that logs it: one lapse, one event, one increment,
        // however many agents run `lease ls`. `pact.lease.peer` is the holder
        // whose claim lapsed — equal to `pact.agent` when an agent sweeps its
        // own, which is how you tell "I abandoned a file" from "someone else
        // did". (pact-aw7.9: in `pact ui` these buffer until the TUI exits.)
        count_transition("expired");
        record_hold(lease, "expired");
        log_event(
            repo_root,
            &lease.agent,
            "expired",
            &lease.path,
            Some(format!(
                "lease lapsed (ttl {}s), lock collected",
                lease.ttl_secs
            )),
            lease.ttl_secs,
            None,
            // The holder's own recorded context, so this row says something true about
            // the lease it closes. `None` on a lock written before pact recorded it,
            // which correctly falls back to the sweeper's rather than inventing one.
            lease.invoked_from.clone(),
        );
    }
}

/// What a sweep did to one hold.
#[derive(Debug, Clone, Serialize)]
pub struct Swept {
    pub path: String,
    /// The agent whose hold this was.
    pub holder: String,
    /// Seconds past its own TTL, when it had lapsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub past_ttl_secs: Option<i64>,
    /// Seconds since that holder's last event of any kind, when the log knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder_silent_secs: Option<i64>,
    /// Reclaimed, or left alone because the holder still looks alive.
    pub reclaimed: bool,
}

/// Why a hold was eligible to be swept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sweep {
    /// Only holds past their own TTL. Nobody's, by the lease's own terms.
    Expired,
    /// Also holds still inside their TTL whose holder has gone silent for
    /// longer than half of it — what `lease ls` labels SUSPECT.
    Suspect,
}

/// Reclaim holds whose holder is gone, and record that it was a RECOVERY
/// (pact-51g, pact-dyo).
///
/// ## Why this is not `--steal`
///
/// `--steal` overrides a live claim on the caller's assertion that the holder
/// is gone. It writes `displaced` + `stolen`, which is exactly what trampling a
/// working peer writes, so `pact audit --check double-win` reports both
/// identically. Measured over one 12-agent run: **six double-wins, every one a
/// steal against a peer that had genuinely died.** A fleet's most responsible
/// behaviour appeared in the audit as its worst, and nothing in the log could
/// separate them.
///
/// This reclaims on pact's own evidence instead of the caller's word, and
/// records that evidence: how far past TTL the hold was, and how long its holder
/// had been silent. A reader — or `--check double-win` — can then tell recovery
/// from trampling.
///
/// ## Why `Sweep::Expired` alone would not have helped
///
/// The bead that asked for this proposed sweeping expired holds. That is the
/// safe case and it is the default here, but it would not have prevented one of
/// those six: every one was a hold still INSIDE its 45-minute TTL (32 minutes,
/// 24 minutes, 19 minutes) whose holder had died. Recovering those is what a
/// fleet actually needs, which is what [`Sweep::Suspect`] is for — and why it is
/// opt-in rather than the default, because a silent holder may yet come back
/// and an expired one is nobody's by definition.
pub fn sweep(repo_root: &Path, agent: &str, mode: Sweep, paths: &[String]) -> Result<Vec<Swept>> {
    let wanted: Vec<String> = paths.iter().map(|p| normalize_path(repo_root, p)).collect();

    let mut swept = Vec::new();
    for (lock_path, entry) in scan(repo_root)? {
        let lease = &entry.lease;
        if !wanted.is_empty() && !wanted.contains(&lease.path) {
            continue;
        }
        // Never your own: sweeping is for holders who cannot release, and
        // `release` is right there for the ones who can.
        if lease.agent == agent {
            continue;
        }

        let eligible = entry.expired || (mode == Sweep::Suspect && entry.suspect);
        let past_ttl = entry
            .expired
            .then(|| -entry.remaining_secs)
            .filter(|s| *s > 0);

        if !eligible {
            swept.push(Swept {
                path: lease.path.clone(),
                holder: lease.agent.clone(),
                past_ttl_secs: past_ttl,
                holder_silent_secs: entry.holder_silent_secs,
                reclaimed: false,
            });
            continue;
        }

        if std::fs::remove_file(&lock_path).is_err() {
            // Another sweeper won the unlink. One lapse, one event.
            continue;
        }
        count_transition("reclaimed");
        record_hold(lease, "reclaimed");
        let evidence = match (past_ttl, entry.holder_silent_secs) {
            (Some(past), _) => format!(
                "lapsed {} past its {}s ttl",
                human_secs(past),
                lease.ttl_secs
            ),
            (None, Some(silent)) => format!(
                "holder silent {} against a {}s ttl",
                human_secs(silent),
                lease.ttl_secs
            ),
            (None, None) => "holder never seen in the event log".to_string(),
        };
        // The SWEEPER is the agent here, unlike `expired`, which describes the
        // holder because nobody chose it. A reclaim is somebody's deliberate
        // act and the log has to say whose.
        log_event(
            repo_root,
            agent,
            "reclaimed",
            &lease.path,
            Some(format!("reclaimed from {}: {evidence}", lease.agent)),
            lease.ttl_secs,
            None,
            lease.invoked_from.clone(),
        );
        swept.push(Swept {
            path: lease.path.clone(),
            holder: lease.agent.clone(),
            past_ttl_secs: past_ttl,
            holder_silent_secs: entry.holder_silent_secs,
            reclaimed: true,
        });
    }
    swept.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(swept)
}

/// Release every lease held by `agent`, so "release everything I hold" is one
/// call that cannot be half-forgotten (pact-rnc.8). Returns the released paths,
/// sorted; holding nothing is success with an empty Vec.
///
/// Only leases that were genuinely held are reported (pact-rnc.24). An expired
/// lease was already nobody's, so calling its removal a "release" overstates
/// what happened — the same dishonesty as pact-rnc.8, where agents announced
/// releases that never occurred. The agent's own expired lock files are still
/// deleted from disk (leaving them behind would leak a lock nobody owns); they
/// are just not claimed as releases in the report.
///
/// Reads via [`peek`], not [`list`]: `list`'s GC used to unlink the expired
/// locks mid-iteration, after which `release` found nothing and returned
/// `Ok(None)` — and the path was printed as released anyway.
pub fn release_all(repo_root: &Path, agent: &str) -> Result<Vec<String>> {
    current_store().release_all(repo_root, agent)
}

fn release_all_fs(repo_root: &Path, agent: &str) -> Result<Vec<String>> {
    let mut held = Vec::new();
    let mut expired = Vec::new();
    for entry in peek_fs(repo_root, true)? {
        if entry.lease.agent != agent {
            continue;
        }
        if entry.expired {
            expired.push(entry.lease);
        } else {
            held.push(entry.lease.path);
        }
    }
    held.sort();

    // Filtered on what release_fs actually did, not on what `peek` predicted.
    // `peek` runs first and the TTL keeps running: a lease that was live at the
    // read can lapse before its turn comes, and reporting it as released would
    // be the same overstatement pact-rnc.24 removed — now detectable, because
    // `ReleaseOutcome` distinguishes the cases (pact-mqw.7).
    let mut held: Vec<String> = held
        .into_iter()
        // `release_relative`, NOT `release_fs`: these paths came out of the lease
        // store and are already repo-relative. See that function for what
        // re-normalizing them cost in the field (finding 2).
        .filter_map(
            |path| match release_relative(repo_root, agent, &path, false) {
                Ok(o) if o.removed_a_lock() => Some(Ok(path)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            },
        )
        .collect::<Result<Vec<String>>>()?;
    held.sort();
    // Swept, not reported — and deliberately not via `release`, which would
    // append a "released" event and put the same overstatement back into the
    // activity feed. It is logged as what it actually was, an "expired" lease
    // collected. Best-effort: a lock we cannot remove is not a reason to fail
    // "release everything", and nothing in the fleet respects it anyway.
    for lease in &expired {
        if let Ok(lock_path) = lock_file_path(repo_root, &lease.path) {
            collect_expired(repo_root, &lock_path, lease);
        }
    }
    // "Release everything" includes the telemetry breadcrumbs, which are the
    // only thing in `.pact/` that nothing else ever collects.
    sweep_wait_markers(repo_root, agent);
    Ok(held)
}

/// Refresh `acquired_at` on a lease `agent` already holds, so a long task can
/// outlive its TTL on purpose instead of by accident (pact-rnc.9).
/// Deliberately does NOT create a missing lease: a typo'd path must not
/// silently claim something new.
pub fn renew(repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo> {
    current_store().renew(repo_root, agent, path)
}

fn renew_fs(repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo> {
    let relative = normalize_path(repo_root, path);
    let lock_path = lock_file_path(repo_root, &relative)?;

    if !lock_path.exists() {
        // pact-m7j.9.6: same probe as release_fs's miss — a scope/topology
        // change since acquire time can make a live lease resolve somewhere
        // else entirely, which looks identical to "never acquired" from here.
        // Still an error either way: renew's contract is that it never
        // creates a lease, so a genuine miss must fail regardless.
        if let Some(other) = other_candidate_lock_path(repo_root, &relative) {
            if other.exists() {
                anyhow::bail!(
                    "no lease on {relative} to renew at {}, but one exists at {}; did \
                     PACT_WORKTREE_SCOPE or this repository's worktree topology change \
                     since this lease was acquired?",
                    lock_path.display(),
                    other.display()
                );
            }
        }
        anyhow::bail!("no lease on {relative} to renew (use `pact lease acquire` to claim it)");
    }
    // Same guard as acquire_inner's takeover branches (pact-m7j.1.6): renew
    // never received it, so a concurrent steal's write could land between this
    // read and the write below with nothing to catch it.
    let _guard = WriteGuard::acquire(&lock_path)?;
    let existing = read_lease(&lock_path).map_err(|e| {
        // Ownership can't be checked from unparsable content, so renewing it
        // is not safe — but the error must point somewhere, not just repeat
        // `serde_json`'s parse failure back at the caller. `--steal` is
        // exactly the recovery path built for a corrupt lock (pact-m7j.4.2).
        // Same exit code as release_fs's and acquire_inner's corrupt-lock
        // branches: 2, "this path is not available", not the generic 1
        // this used to carry (pact-m7j.4.8).
        exit_with(
            2,
            format!(
                "lock file for {relative} is corrupt and cannot be renewed ({e:#}); \
                 use `pact lease acquire {relative} --steal` to recover it"
            ),
        )
    })?;
    if existing.agent != agent {
        count_transition("conflicted");
        return Err(exit_with(
            2,
            format!(
                "lease on {relative} is held by {}, not {agent}",
                existing.agent
            ),
        ));
    }

    // Re-stamped rather than carried over: a long task can outlive a `git
    // switch`, and a lease claiming a branch the worktree left is a lie that
    // survives every renew.
    let (branch, worktree, _) = worktree_stamp(repo_root);
    let renewed = LeaseInfo {
        acquired_at: effective_now(repo_root).to_rfc3339(),
        branch,
        worktree,
        ..existing
    };
    write_lease_atomic(&lock_path, &renewed)?;
    verify_own_lease(&lock_path, agent)?;
    log_event(
        repo_root,
        agent,
        "renewed",
        &relative,
        renewed.note.clone(),
        renewed.ttl_secs,
        None,
        None,
    );
    count_transition("renewed");
    Ok(renewed)
}

/// Fixtures shared by every submodule's tests: build a repo, claim a path,
/// age a claim, read back what is held. They live here rather than in any one
/// sibling because no sibling owns them — a release test needs to acquire and
/// an acquire test needs to inspect.
#[cfg(test)]
mod testutil {
    use super::*;
    use crate::events;
    use chrono::Duration;
    use chrono::{DateTime, Utc};
    use std::collections::BTreeMap;
    use std::path::Path;

    pub(super) fn lease_aged(ttl_secs: u64, age_secs: i64) -> (LeaseInfo, DateTime<Utc>) {
        let now = Utc::now();
        let acquired = now - Duration::seconds(age_secs);
        (
            LeaseInfo {
                agent: "agent-a".into(),
                path: "x".into(),
                acquired_at: acquired.to_rfc3339(),
                ttl_secs,
                note: None,
                branch: None,
                worktree: None,
                invoked_from: None,
                content_hash: None,
                extra: BTreeMap::new(),
            },
            now,
        )
    }

    pub(super) fn repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    pub(super) fn claim(root: &Path, agent: &str, path: &str) {
        acquire(root, agent, path, DEFAULT_TTL_SECS, false, None).unwrap();
    }

    pub(super) fn held_by(root: &Path, agent: &str) -> Vec<String> {
        let mut paths: Vec<String> = list(root, true)
            .unwrap()
            .into_iter()
            .filter(|e| e.lease.agent == agent)
            .map(|e| e.lease.path)
            .collect();
        paths.sort();
        paths
    }

    /// Plant a lock file that is already `age_secs` old, without going through
    /// `acquire` — the only way to test expiry without sleeping.
    pub(super) fn claim_aged(root: &Path, agent: &str, path: &str, ttl_secs: u64, age_secs: i64) {
        claim_at(
            root,
            agent,
            path,
            ttl_secs,
            Utc::now() - Duration::seconds(age_secs),
        );
    }

    /// Same as `claim_aged`, but takes the exact `acquired_at` instant rather
    /// than an age relative to the real wall clock — needed to plant a lease
    /// relative to a fabricated clock watermark instead of real "now".
    /// An `Event` with every optional field empty, so a test can name only the
    /// three or four fields it is actually about. `Event` has no `Default` on
    /// purpose — every production writer should have to think about each field —
    /// but a test planting a fixture is not that.
    pub(super) fn blank_event() -> events::Event {
        events::Event {
            at: String::new(),
            agent: String::new(),
            kind: String::new(),
            path: None,
            detail: None,
            ttl_secs: None,
            covers_lines: None,
            actor: None,
            displaced: None,
            chain_hash: None,
            invoked_from: None,
            collected_from: None,
            scope: None,
            pact_version: None,
            content_hash: None,
            subscriber: None,
            message_id: None,
            protocol_hash: None,
            head: None,
            holder: None,
            holder_remaining_secs: None,
            holder_branch: None,
            holder_worktree: None,
            ..Default::default()
        }
    }

    pub(super) fn claim_at(
        root: &Path,
        agent: &str,
        path: &str,
        ttl_secs: u64,
        acquired_at: DateTime<Utc>,
    ) {
        let lease = LeaseInfo {
            agent: agent.into(),
            path: path.into(),
            acquired_at: acquired_at.to_rfc3339(),
            ttl_secs,
            note: None,
            branch: None,
            worktree: None,
            invoked_from: None,
            content_hash: None,
            extra: BTreeMap::new(),
        };
        write_lease_atomic(&lock_file_path(root, path).unwrap(), &lease).unwrap();
    }

    pub(super) fn lock_exists(root: &Path, path: &str) -> bool {
        lock_file_path(root, path).unwrap().exists()
    }

    // ---- pact-rnc.19: peek() answers without mutating -------------------

    pub(super) fn event_kinds(root: &Path) -> Vec<(String, String)> {
        crate::events::recent(root, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.path.unwrap_or_default()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::testutil::*;
    use chrono::Duration;

    /// pact-51g/pact-dyo. The default is the safe case: a hold past its own TTL
    /// is nobody's by the lease's own terms.
    #[test]
    fn sweep_reclaims_a_lapsed_hold_and_leaves_live_ones_alone() {
        let tmp = repo();
        let root = tmp.path();
        claim_at(
            root,
            "dead",
            "lapsed.rs",
            120,
            Utc::now() - Duration::seconds(600),
        );
        acquire(root, "busy", "working.rs", 2700, false, None).unwrap();

        let swept = sweep(root, "rescuer", Sweep::Expired, &[]).unwrap();
        let taken: Vec<_> = swept.iter().filter(|s| s.reclaimed).collect();
        assert_eq!(taken.len(), 1, "{swept:?}");
        assert_eq!(taken[0].path, "lapsed.rs");
        assert_eq!(taken[0].holder, "dead");

        // The working peer is reported, not touched — an agent that swept
        // nothing needs to know which of the two reasons applied.
        let left: Vec<_> = swept.iter().filter(|s| !s.reclaimed).collect();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, "working.rs");
        assert!(peek(root, false)
            .unwrap()
            .iter()
            .any(|e| e.lease.path == "working.rs"));
    }

    /// The case `--expired` alone would not have caught, and the reason
    /// `--suspect` exists: every double-win in the millrace run was a steal of a
    /// hold still INSIDE its TTL whose holder had died.
    #[test]
    fn sweep_suspect_reclaims_a_hold_inside_its_ttl_whose_holder_went_silent() {
        let tmp = repo();
        let root = tmp.path();
        let ttl = 600u64;
        let long_ago = Utc::now() - Duration::seconds(400);
        claim_at(root, "stalled", "printer.rs", ttl, long_ago);
        events::append(
            root,
            &events::Event {
                at: long_ago.to_rfc3339(),
                agent: "stalled".to_string(),
                kind: "acquired".to_string(),
                path: Some("printer.rs".to_string()),
                ttl_secs: Some(ttl),
                ..blank_event()
            },
        );

        // Not lapsed, so the safe mode leaves it.
        let safe = sweep(root, "rescuer", Sweep::Expired, &[]).unwrap();
        assert!(!safe[0].reclaimed, "still inside its ttl: {safe:?}");

        let swept = sweep(root, "rescuer", Sweep::Suspect, &[]).unwrap();
        assert!(swept[0].reclaimed, "{swept:?}");
        assert_eq!(swept[0].holder, "stalled");
        assert!(swept[0].holder_silent_secs.unwrap() >= 400);
    }

    /// The whole point: a reclaim must not look like a steal in the log.
    #[test]
    fn a_reclaim_is_recorded_under_the_sweeper_and_names_its_evidence() {
        let tmp = repo();
        let root = tmp.path();
        claim_at(
            root,
            "dead",
            "x.rs",
            120,
            Utc::now() - Duration::seconds(600),
        );
        sweep(root, "rescuer", Sweep::Expired, &[]).unwrap();

        let (events, _) = events::numbered(root).unwrap();
        let ev = events
            .iter()
            .map(|(_, e)| e)
            .find(|e| e.kind == "reclaimed")
            .expect("a reclaim must leave a reclaimed event");
        assert_eq!(
            ev.agent, "rescuer",
            "the SWEEPER owns this row, not the holder"
        );
        assert_eq!(ev.path.as_deref(), Some("x.rs"));
        let detail = ev.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("dead"),
            "it must name whose hold it was: {detail}"
        );
        assert!(detail.contains("ttl"), "and the evidence: {detail}");
        // Not a steal: `stolen`/`displaced` are what --check double-win reads.
        assert!(
            !events
                .iter()
                .any(|(_, e)| e.kind == "stolen" || e.kind == "displaced"),
            "a reclaim must not write the events a steal writes"
        );
    }

    /// Sweeping your own hold is release's job, and quietly doing it here would
    /// let an agent "recover" from itself.
    #[test]
    fn sweep_never_touches_the_sweepers_own_hold() {
        let tmp = repo();
        let root = tmp.path();
        claim_at(
            root,
            "me",
            "mine.rs",
            120,
            Utc::now() - Duration::seconds(600),
        );
        let swept = sweep(root, "me", Sweep::Suspect, &[]).unwrap();
        assert!(swept.is_empty(), "{swept:?}");
        assert_eq!(
            peek(root, true).unwrap().len(),
            1,
            "the lock is still there"
        );
    }

    /// Named paths limit the sweep, so recovering one file does not silently
    /// reclaim every abandoned hold in the repository.
    #[test]
    fn sweep_can_be_limited_to_named_paths() {
        let tmp = repo();
        let root = tmp.path();
        let old = Utc::now() - Duration::seconds(600);
        claim_at(root, "dead", "a.rs", 120, old);
        claim_at(root, "dead", "b.rs", 120, old);

        let swept = sweep(root, "rescuer", Sweep::Expired, &["a.rs".to_string()]).unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].path, "a.rs");
        assert!(peek(root, true)
            .unwrap()
            .iter()
            .any(|e| e.lease.path == "b.rs"));
    }

    #[test]
    fn release_all_releases_only_the_callers_leases() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "src/b.rs");
        claim(root, "agent-a", "src/a.rs");
        claim(root, "agent-b", "src/other.rs");

        assert_eq!(
            release_all(root, "agent-a").unwrap(),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
        assert!(held_by(root, "agent-a").is_empty());
        assert_eq!(held_by(root, "agent-b"), vec!["src/other.rs".to_string()]);
    }

    #[test]
    fn release_all_with_nothing_held_succeeds_empty() {
        let tmp = repo();
        claim(tmp.path(), "agent-b", "src/other.rs");
        assert!(release_all(tmp.path(), "agent-a").unwrap().is_empty());
        assert_eq!(
            held_by(tmp.path(), "agent-b"),
            vec!["src/other.rs".to_string()]
        );
    }

    #[test]
    fn release_reports_the_displaced_holder_only_when_forced() {
        let tmp = repo();
        let root = tmp.path();

        claim(root, "agent-a", "mine.rs");
        let first = release(root, "agent-a", "mine.rs", false).unwrap();
        assert!(matches!(
            first,
            ReleaseOutcome::Released {
                past_ttl_secs: None
            }
        ));
        assert_eq!(first.displaced(), None);
        // Idempotent, and now distinguishable: a repeat release removed nothing
        // and there is no expiry of this agent's on record (pact-mqw.7).
        assert!(matches!(
            release(root, "agent-a", "mine.rs", false).unwrap(),
            ReleaseOutcome::NothingHeld
        ));

        claim(root, "agent-a", "theirs.rs");
        assert!(release(root, "agent-b", "theirs.rs", false).is_err());
        let forced = release(root, "agent-b", "theirs.rs", true).unwrap();
        assert_eq!(forced.displaced(), Some("agent-a"));
    }

    /// pact-mqw.7: the crucible shape. A lease lapses, `lease ls` collects the
    /// lock, the agent commits and only then releases — and used to be told it had
    /// released cleanly. The window it was actually unprotected for is the one
    /// fact it needs, and the only place that fact survives is the event log.
    #[test]
    fn releasing_a_lapsed_lease_reports_the_lapse_instead_of_success() {
        let tmp = repo();
        let root = tmp.path();

        // A lease with a TTL short enough to lapse, backdated past ttl+grace so
        // `list`'s sweep collects it exactly as a peer's `lease ls` would.
        let ttl = 600u64;
        claim_at(
            root,
            "agent-08",
            "tests/corpus.rs",
            ttl,
            Utc::now() - Duration::seconds(ttl as i64 + GRACE_SECS + 90),
        );
        // The sweep: this is what leaves no lock file behind.
        let _ = list(root, false).unwrap();

        let outcome = release(root, "agent-08", "tests/corpus.rs", false).unwrap();
        let ReleaseOutcome::AlreadyExpired { at, since_secs, .. } = &outcome else {
            panic!("a lapsed-and-collected lease must not report as released: {outcome:?}");
        };
        assert!(!at.is_empty(), "the lapse must be dated: {outcome:?}");
        assert!(
            since_secs.unwrap_or(-1) >= 0,
            "and the free window measured: {outcome:?}"
        );
        assert!(!outcome.removed_a_lock());

        // A DIFFERENT agent asking about the same path gets `nothing-held`, not
        // somebody else's expiry: the question is "did I overrun", and a peer's
        // lapse says nothing about that.
        assert!(matches!(
            release(root, "agent-09", "tests/corpus.rs", false).unwrap(),
            ReleaseOutcome::NothingHeld
        ));
    }

    /// The other half: the lock survived, so it really is a release — but the
    /// holder ran past its own TTL and only luck kept a peer from reclaiming.
    #[test]
    fn releasing_an_overrun_lease_says_how_far_past_its_ttl_it_ran() {
        let tmp = repo();
        let root = tmp.path();
        // Past the ttl but inside the grace window, so nothing has swept it.
        let ttl = 600u64;
        claim_at(
            root,
            "agent-05",
            "src/ast.rs",
            ttl,
            Utc::now() - Duration::seconds(ttl as i64 + 10),
        );

        let outcome = release(root, "agent-05", "src/ast.rs", false).unwrap();
        let ReleaseOutcome::Released { past_ttl_secs } = &outcome else {
            panic!("the lock was still there, so this IS a release: {outcome:?}");
        };
        assert!(
            past_ttl_secs.unwrap_or(0) >= 10,
            "and it must say how late: {outcome:?}"
        );
        assert!(outcome.removed_a_lock());
    }

    #[test]
    fn renew_refreshes_acquired_at_for_the_holder() {
        let tmp = repo();
        let root = tmp.path();
        let first = acquire(root, "agent-a", "f.rs", 42, false, Some("note".into()))
            .unwrap()
            .lease;

        let renewed = renew(root, "agent-a", "f.rs").unwrap();
        assert_ne!(renewed.acquired_at, first.acquired_at);
        assert_eq!(renewed.ttl_secs, 42, "renew keeps the original ttl");
        assert_eq!(renewed.note.as_deref(), Some("note"));
        assert_eq!(
            read_lease(&lock_file_path(root, "f.rs").unwrap())
                .unwrap()
                .acquired_at,
            renewed.acquired_at,
            "renew persists to disk"
        );
    }

    /// pact-m7j.9.8: `renew_fs` reads a lease, re-stamps three fields
    /// (`acquired_at`, `branch`, `worktree`), and writes the whole struct
    /// back. A lock file written by a NEWER binary can carry a field this
    /// compiled `LeaseInfo` has never heard of — the same shape a pre-0.5.0
    /// binary's lock file has relative to today's `branch`/`worktree`, just
    /// one field further in the future. Without a catch-all, that
    /// read-modify-write silently drops it; `#[serde(flatten)] extra` must
    /// carry it through untouched.
    #[test]
    fn renew_preserves_a_field_the_current_struct_does_not_declare() {
        let tmp = repo();
        let root = tmp.path();
        let lock_path = lock_file_path(root, "f.rs").unwrap();

        // A lock file shaped like a lease written by a binary newer than this
        // one: everything `renew_fs` knows about, plus a field it does not.
        let raw = serde_json::json!({
            "agent": "agent-a",
            "path": "f.rs",
            "acquired_at": (Utc::now() - Duration::seconds(10)).to_rfc3339(),
            "ttl_secs": 900,
            "note": null,
            "branch": "feat/x",
            "worktree": "wt-auth",
            "future_field": "some-value",
        });
        std::fs::write(&lock_path, serde_json::to_string(&raw).unwrap()).unwrap();

        let renewed = renew(root, "agent-a", "f.rs").unwrap();
        assert_eq!(
            renewed.extra.get("future_field"),
            Some(&serde_json::Value::String("some-value".into())),
            "renew must not silently drop a field it doesn't declare"
        );

        let on_disk = read_lease(&lock_path).unwrap();
        assert_eq!(
            on_disk.extra.get("future_field"),
            Some(&serde_json::Value::String("some-value".into())),
            "the unknown field must survive on disk, not just on the in-memory value"
        );
    }

    #[test]
    fn renew_without_an_existing_lease_errors() {
        let tmp = repo();
        assert!(renew(tmp.path(), "agent-a", "typo.rs").is_err());
        assert!(
            !lock_file_path(tmp.path(), "typo.rs").unwrap().exists(),
            "renew must not create a lease"
        );
    }

    #[test]
    fn renew_of_another_agents_lease_exits_2() {
        let tmp = repo();
        claim(tmp.path(), "agent-a", "f.rs");
        let err = renew(tmp.path(), "agent-b", "f.rs").unwrap_err();
        assert_eq!(crate::output::code_for(&err), 2);
        assert_eq!(
            read_lease(&lock_file_path(tmp.path(), "f.rs").unwrap())
                .unwrap()
                .agent,
            "agent-a"
        );
    }

    /// pact-m7j.7.1: `existing.agent == agent` is a string comparison against a
    /// self-asserted `PACT_AGENT`, with no PID or session behind it — and every
    /// real caller of `acquire` is a fresh CLI process per invocation (verified
    /// via `grep` across the codebase: only `main.rs`'s `lease acquire` dispatch
    /// calls it in production; nothing long-lived re-acquires within one
    /// process). Two `acquire()` calls in this test stand in for exactly that:
    /// two independent process invocations that happen to export the same
    /// `PACT_AGENT`. This characterizes the documented behaviour (see
    /// docs/leases.md, "The trust boundary") rather than a bug being fixed
    /// here — a PID/session field was investigated and rejected because it
    /// would only ever misfire on the ordinary refresh workflow, not catch a
    /// genuine collision. If this test ever fails, docs/leases.md's trust
    /// section needs to change with it.
    #[test]
    fn reentrant_refresh_silently_overwrites_regardless_of_calling_process() {
        let tmp = repo();
        let root = tmp.path();

        let first = acquire(
            root,
            "agent-a",
            "f.rs",
            3600,
            false,
            Some("first process, long ttl".into()),
        )
        .unwrap();
        assert!(!first.stolen);

        // A second, distinguishable invocation (different note, much shorter
        // ttl) with the identical `PACT_AGENT` string.
        let second = acquire(
            root,
            "agent-a",
            "f.rs",
            30,
            false,
            Some("second process, different note".into()),
        )
        .unwrap();

        // Succeeds silently: no error, no `stolen` flag — indistinguishable
        // from genuine self-renewal, which is exactly the documented gap.
        assert!(!second.stolen);
        assert_eq!(second.lease.ttl_secs, 30, "the shorter ttl silently wins");
        assert_eq!(
            second.lease.note.as_deref(),
            Some("second process, different note"),
            "the first process's note is silently overwritten, with no warning"
        );

        // The event log is the one place this is distinguishable at all: a
        // `renewed` kind, not `acquired` — auditable, but not a warning.
        assert_eq!(
            event_kinds(root),
            vec![
                ("acquired".to_string(), "f.rs".to_string()),
                ("renewed".to_string(), "f.rs".to_string()),
            ]
        );
    }

    #[test]
    fn acquire_many_takes_every_path_or_none() {
        let tmp = repo();
        let root = tmp.path();
        let paths = vec!["src/agents.rs".to_string(), "src/main.rs".to_string()];

        let outcomes =
            acquire_many(root, "agent-a", &paths, 900, false, Some("mod line".into())).unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| !o.stolen));
        assert_eq!(held_by(root, "agent-a"), paths);
        assert!(outcomes
            .iter()
            .all(|o| o.lease.note.as_deref() == Some("mod line")));
    }

    #[test]
    fn acquire_many_rolls_back_everything_it_took_on_conflict() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-b", "src/main.rs");

        let err = acquire_many(
            root,
            "agent-a",
            &[
                "src/agents.rs".to_string(),
                "src/main.rs".to_string(),
                "src/never.rs".to_string(),
            ],
            900,
            false,
            None,
        )
        .unwrap_err();

        // The error names the path that actually has to be negotiated over.
        assert!(
            err.to_string().contains("src/main.rs"),
            "error should name the contended path: {err}"
        );
        assert_eq!(crate::output::code_for(&err), 2);

        // Nothing taken, nothing left behind, and the other agent is untouched.
        assert!(
            held_by(root, "agent-a").is_empty(),
            "rollback must leave agent-a holding nothing"
        );
        assert!(
            !lock_exists(root, "src/agents.rs"),
            "stray lock file left on disk after rollback"
        );
        assert!(!lock_exists(root, "src/never.rs"), "never reached");
        assert_eq!(held_by(root, "agent-b"), vec!["src/main.rs".to_string()]);
    }

    /// A failed multi-claim must not destroy a claim the agent walked in with —
    /// and "the claim", not "some claim on the same path": the TTL and the note
    /// are what peers use to decide whether the file is reclaimable, so a batch
    /// that reports "nothing was taken" while having downgraded a 900s claim with
    /// a note to a 30s claim without one has destroyed it just as effectively
    /// (pact-rnc.21).
    #[test]
    fn acquire_many_rollback_keeps_a_lease_the_agent_already_held() {
        let tmp = repo();
        let root = tmp.path();
        let before = acquire(
            root,
            "agent-a",
            "src/mine.rs",
            900,
            false,
            Some("IMPORTANT: refactor in progress".into()),
        )
        .unwrap()
        .lease;
        claim(root, "agent-b", "src/theirs.rs");

        assert!(acquire_many(
            root,
            "agent-a",
            &["src/mine.rs".to_string(), "src/theirs.rs".to_string()],
            30, // the downgrade a failed batch used to leave behind
            false,
            None,
        )
        .is_err());

        assert_eq!(
            held_by(root, "agent-a"),
            vec!["src/mine.rs".to_string()],
            "rollback released a pre-existing lease"
        );
        let after = read_lease(&lock_file_path(root, "src/mine.rs").unwrap()).unwrap();
        assert_eq!(after.ttl_secs, 900, "the batch's --ttl overwrote the lease");
        assert_eq!(
            after.note.as_deref(),
            Some("IMPORTANT: refactor in progress"),
            "the batch erased the note peers read"
        );
        assert_eq!(
            after.acquired_at, before.acquired_at,
            "the batch reset the lease's age, so peers see it as fresher than it is"
        );
    }

    /// pact-m7j.1.2: the rollback above restores the pre-batch lease on disk,
    /// but until now left no trace that it had — the feed's last word on the
    /// path stayed the "renewed" event the refresh logged, never retracted or
    /// explained. A "restored" event must land, naming the path and the
    /// pre-batch `acquired_at` it put back.
    #[test]
    fn acquire_many_rollback_logs_the_restoration() {
        let tmp = repo();
        let root = tmp.path();
        let before = acquire(root, "agent-a", "src/mine.rs", 900, false, None)
            .unwrap()
            .lease;
        claim(root, "agent-b", "src/theirs.rs");

        assert!(acquire_many(
            root,
            "agent-a",
            &["src/mine.rs".to_string(), "src/theirs.rs".to_string()],
            30,
            false,
            None,
        )
        .is_err());

        assert_eq!(
            event_kinds(root),
            vec![
                ("acquired".to_string(), "src/mine.rs".to_string()),
                ("acquired".to_string(), "src/theirs.rs".to_string()),
                ("renewed".to_string(), "src/mine.rs".to_string()),
                // pact-juz.1: the batch's second path was refused (agent-b
                // already holds it), which is what triggers the rollback
                // below.
                ("refused".to_string(), "src/theirs.rs".to_string()),
                ("restored".to_string(), "src/mine.rs".to_string()),
            ],
            "the refresh's renewed event must be followed by a restored event, \
             not left as the feed's last, uncorrected word on the path"
        );
        let restored = crate::events::recent(root, 1).unwrap();
        assert!(
            restored[0]
                .detail
                .as_deref()
                .unwrap()
                .contains(&before.acquired_at),
            "the restored event should name the pre-batch acquired_at it put back: {:?}",
            restored[0].detail
        );
    }

    // ---- pact-rnc.24: release_all reports only real releases -------------

    /// FINDING 5a, the data bug: an `expired` row must describe the HOLDER's lease, not the
    /// process that swept the lock.
    ///
    /// Measured in the quern run, 2 of 3 expiries carried a worktree attribution that was
    /// not the holder's, because the row is written by whoever happens to run `lease ls`
    /// later — often in the main checkout, minutes after the holder is gone. No later fix
    /// repairs a log already on disk, which is why this outranked the check it was
    /// breaking.
    #[test]
    fn an_expiry_carries_the_holders_context_and_names_the_sweeper_separately() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "f.rs");

        // This fixture has no linked worktrees, so the lock records no invocation context
        // at all — see `worktree_stamp` for why that is deliberate. What is under test is
        // what `collect_expired` does with a context when there IS one, so the lapsed lock
        // carries a sentinel the sweeper could not possibly produce.
        let lock = read_lease(&lock_file_path(root, "f.rs").unwrap()).unwrap();
        assert_eq!(
            lock.invoked_from, None,
            "a repo with no worktrees keeps pre-worktree lock files byte-identical"
        );
        let mut lapsed = lock.clone();
        lapsed.invoked_from = Some("wt-holder".to_string());
        collect_expired(root, &lock_file_path(root, "f.rs").unwrap(), &lapsed);

        let expired = crate::events::recent(root, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "expired")
            .expect("an expiry was logged");
        assert_eq!(
            expired.invoked_from.as_deref(),
            Some("wt-holder"),
            "the expiry must carry the HOLDER's context, not the sweeper's"
        );
        assert!(
            expired.collected_from.is_some()
                && expired.collected_from.as_deref() != Some("wt-holder"),
            "the sweeper must be recorded separately, where topology ignores it: {:?}",
            expired.collected_from
        );
    }

    #[test]
    fn release_all_omits_expired_leases_but_still_sweeps_them() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "live.rs");
        claim_aged(root, "agent-a", "dead.rs", 100, 100 + GRACE_SECS + 1);

        assert_eq!(
            release_all(root, "agent-a").unwrap(),
            vec!["live.rs".to_string()],
            "an expired lease was already nobody's; do not claim it as released"
        );
        assert!(!lock_exists(root, "live.rs"));
        assert!(
            !lock_exists(root, "dead.rs"),
            "the expired lock must still be swept from disk, just not reported"
        );
    }

    // ---- pact-rnc.13: every transition lands in the activity log ---------

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

    #[test]
    fn a_denied_acquire_logs_a_refused_event_naming_the_holder() {
        let tmp = repo();
        let root = tmp.path();

        acquire(root, "agent-a", "hot.rs", 900, false, None).unwrap();
        let denied = acquire(
            root,
            "agent-b",
            "hot.rs",
            900,
            false,
            Some("my turn".into()),
        );
        assert!(denied.is_err(), "a live, non-expired lease must refuse");

        assert_eq!(
            event_kinds(root),
            vec![
                ("acquired".to_string(), "hot.rs".to_string()),
                ("refused".to_string(), "hot.rs".to_string()),
            ]
        );

        let events = crate::events::recent(root, 100).unwrap();
        let refused = &events[1];
        assert_eq!(
            refused.agent, "agent-b",
            "logged under the requester, matching every other kind's convention"
        );
        let detail = refused.detail.as_deref().unwrap();
        assert!(detail.contains("agent-a"), "must name the holder: {detail}");
        assert!(
            detail.contains("remaining"),
            "must carry the holder's remaining TTL: {detail}"
        );
        assert!(
            detail.contains("my turn"),
            "must carry the requester's own note, if given: {detail}"
        );
    }

    /// Taking over a dead claim is logged, and says so.
    #[test]
    fn taking_over_an_expired_lease_logs_a_steal_naming_the_previous_holder() {
        let tmp = repo();
        let root = tmp.path();
        claim_aged(root, "agent-a", "dead.rs", 100, 100 + GRACE_SECS + 1);

        assert!(
            acquire(root, "agent-b", "dead.rs", 900, false, None)
                .unwrap()
                .stolen
        );

        // Two rows: the previous claim ending, then the takeover (see
        // `a_reclaim_logs_the_previous_holders_expiry_before_the_takeover`).
        let events = crate::events::recent(root, 10).unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[1].kind, "stolen");
        assert_eq!(events[1].agent, "agent-b");
        let detail = events[1].detail.clone().unwrap();
        assert!(detail.contains("agent-a"), "{detail}");
        assert!(detail.contains("expired"), "{detail}");
    }

    /// pact-rnc.13: the crashed-agent case. An agent dies holding a file, its TTL
    /// lapses, someone runs `lease ls` and the lock is collected — and before
    /// this, the feed's last word was still "agent-a acquired gone.rs", so `pact
    /// log` named a dead agent as the current holder of a file whose lock was
    /// already gone.
    #[test]
    fn collecting_an_expired_lock_logs_the_lapse_against_its_holder() {
        let tmp = repo();
        let root = tmp.path();
        claim_aged(root, "agent-a", "gone.rs", 60, 60 + GRACE_SECS + 1);

        // Whoever happens to run the command that collects it.
        assert!(list(root, false).unwrap().is_empty());
        assert!(!lock_exists(root, "gone.rs"), "list must still GC");

        let events = crate::events::recent(root, 10).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, "expired");
        assert_eq!(
            events[0].agent, "agent-a",
            "the event belongs to the holder whose lease lapsed, not the collector"
        );
        assert_eq!(events[0].path.as_deref(), Some("gone.rs"));

        // The lock is gone, so a second listing has nothing left to report: one
        // lapse, one event, however many agents run `lease ls`.
        assert!(list(root, true).unwrap().is_empty());
        assert_eq!(crate::events::recent(root, 10).unwrap().len(), 1);
    }

    /// `release --all` sweeps the caller's own expired locks. That is not a
    /// release (pact-rnc.24) — it is an expiry, and it is logged as one.
    #[test]
    fn release_all_logs_its_expired_sweep_as_an_expiry_not_a_release() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "live.rs");
        claim_aged(root, "agent-a", "dead.rs", 100, 100 + GRACE_SECS + 1);

        assert_eq!(release_all(root, "agent-a").unwrap(), vec!["live.rs"]);
        assert_eq!(
            event_kinds(root),
            vec![
                ("acquired".to_string(), "live.rs".to_string()),
                ("released".to_string(), "live.rs".to_string()),
                ("expired".to_string(), "dead.rs".to_string()),
            ],
            "the swept lock must not be reported as a release"
        );
    }

    /// Taking over a dead claim ends the previous one, and the feed says so — the
    /// only way a consumer grouping by `kind` can tell a routine reclaim from a
    /// `--steal` override of a live claim.
    #[test]
    fn a_reclaim_logs_the_previous_holders_expiry_before_the_takeover() {
        let tmp = repo();
        let root = tmp.path();
        claim_aged(root, "agent-a", "dead.rs", 60, 60 + GRACE_SECS + 1);

        acquire(root, "agent-b", "dead.rs", 900, false, None).unwrap();

        let events = crate::events::recent(root, 10).unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["expired", "stolen"]
        );
        assert_eq!(events[0].agent, "agent-a");
        assert_eq!(events[1].agent, "agent-b");

        // A --steal of a LIVE claim closes the victim's window too, but with
        // "displaced" rather than "expired": both close, and which one appears
        // is how a consumer grouping by `kind` tells a routine reclaim from a
        // forced override without parsing prose (pact-mqw.1, pact-mqw.2).
        acquire(root, "agent-c", "dead.rs", 900, true, None).unwrap();
        let events = crate::events::recent(root, 10).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["expired", "stolen", "stolen", "displaced"]);

        // The closing row belongs to the agent whose claim ENDED, not to the one
        // that ended it — the same ownership rule as "expired" one assertion up.
        assert_eq!(events[3].agent, "agent-b", "displaced names the victim");
        assert_eq!(events[2].agent, "agent-c", "stolen names the thief");
        assert!(
            events[3]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("--steal"),
            "the displaced row says why it ended: {:?}",
            events[3].detail
        );

        // And custody still resolves to the thief: "displaced" is deliberately
        // absent from is_custody()'s allowlist, and the "stolen" row that always
        // follows it is newer, so `--to-owner-of` addresses agent-c either way.
        assert_eq!(
            crate::events::owner_of(root, "dead.rs")
                .unwrap()
                .map(|o| o.agent),
            Some("agent-c".to_string())
        );
    }

    // ---- pact-aw7.3: what the lease metrics are computed from -------------

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

    /// `verify_own_lease` must reject when a concurrent agent's rename landed
    /// after ours. Simulates the "lost steal" case: we wrote agent-a's data,
    /// but by the time we verify, agent-b has already renamed their file on
    /// top of ours.
    #[test]
    fn verify_own_lease_rejects_when_a_concurrent_rename_overwrote_ours() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "raced.rs");
        let lock_path = lock_file_path(root, "raced.rs").unwrap();

        // agent-b's rename overwrote agent-a's just-written lease.
        let other = LeaseInfo {
            agent: "agent-b".into(),
            path: "raced.rs".into(),
            acquired_at: Utc::now().to_rfc3339(),
            ttl_secs: DEFAULT_TTL_SECS,
            note: None,
            branch: None,
            worktree: None,
            invoked_from: None,
            content_hash: None,
            extra: BTreeMap::new(),
        };
        write_lease_atomic(&lock_path, &other).unwrap();

        // agent-a's verify must now fail with exit 2.
        let err = verify_own_lease(&lock_path, "agent-a").unwrap_err();
        assert_eq!(
            crate::output::code_for(&err),
            2,
            "lost steal must exit 2: {err}"
        );
    }

    /// When two threads race to steal the same expired lease, `WriteGuard`
    /// serializes the whole read-decide-write sequence, so exactly one wins:
    ///   • The agent whose data is on disk at the end always returned `Ok`.
    ///   • The other thread returns exit 2, detected by `verify_own_lease` —
    ///     now a cheap check that the guard worked, not the primary defense.
    ///   • Exactly one thread wins (never zero, never both).
    #[test]
    fn concurrent_double_steal_disk_holder_returned_ok() {
        // Run many iterations to probe different interleavings.
        for _ in 0..20 {
            let tmp = repo();
            let root = tmp.path();
            claim_aged(root, "agent-stale", "hot.rs", 60, 60 + GRACE_SECS + 1);

            let root_a = root.to_path_buf();
            let root_b = root.to_path_buf();

            let handle_a = std::thread::spawn(move || {
                acquire(&root_a, "agent-a", "hot.rs", DEFAULT_TTL_SECS, false, None)
            });
            let handle_b = std::thread::spawn(move || {
                acquire(&root_b, "agent-b", "hot.rs", DEFAULT_TTL_SECS, false, None)
            });

            let res_a = handle_a.join().unwrap();
            let res_b = handle_b.join().unwrap();

            let on_disk = read_lease(&lock_file_path(root, "hot.rs").unwrap()).unwrap();

            // The agent on disk always returned Ok (verify never rejects a true winner).
            match on_disk.agent.as_str() {
                "agent-a" => assert!(
                    res_a.is_ok(),
                    "agent-a is on disk but returned Err: {res_a:?}"
                ),
                "agent-b" => assert!(
                    res_b.is_ok(),
                    "agent-b is on disk but returned Err: {res_b:?}"
                ),
                other => panic!("unexpected agent on disk: {other}"),
            }

            // Exactly one thread won: WriteGuard now serializes the
            // read-decide-write sequence, closing the window
            // verify_own_lease alone could only narrow.
            let successes = [&res_a, &res_b].iter().filter(|r| r.is_ok()).count();
            assert_eq!(
                successes, 1,
                "exactly one of two racers must win: a={res_a:?}, b={res_b:?}"
            );

            // Any loser detected by verify must exit 2, not some other code.
            for res in [&res_a, &res_b] {
                if let Err(e) = res {
                    assert_eq!(
                        crate::output::code_for(e),
                        2,
                        "a losing acquire must exit 2: {e}"
                    );
                }
            }
        }
    }

    /// pact-kqb, found by TLA+ model checking of `WriteGuard`'s first
    /// implementation (a sibling `.guard` file reclaimed once it looked older
    /// than a fixed wall-clock threshold): a LIVE holder legitimately still
    /// inside the critical section — no crash, just contention or an
    /// unusually slow disk — could look "stale" by the same clock and get
    /// preempted by a waiter, reopening the exact double-win this guard
    /// exists to close. TLC's counterexample needed no crash at all, only
    /// time. The fix removed the wall-clock heuristic entirely in favor of a
    /// real `flock(2)`, which the kernel releases only when the holder's file
    /// descriptor actually closes — so there is no elapsed-time threshold to
    /// beat, at any duration. This proves it directly: hold the guard for
    /// six seconds (past the fixed guard's old STALE_GUARD_SECS = 5, the
    /// exact boundary TLC's trace crossed) while alive, and confirm a
    /// concurrent waiter is still blocked at that point, not just eventually
    /// unblocked once the holder actually releases.
    #[test]
    fn a_live_guard_holder_is_never_preempted_no_matter_how_long_it_holds() {
        let tmp = repo();
        let root = tmp.path();
        let lock_path = lock_file_path(root, "hot.rs").unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();

        let (holder_ready_tx, holder_ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder_lock_path = lock_path.clone();
        let holder = std::thread::spawn(move || {
            let _guard = WriteGuard::acquire(&holder_lock_path).unwrap();
            holder_ready_tx.send(()).unwrap();
            // Alive and doing nothing wrong — not crashed, not deadlocked —
            // for longer than the old heuristic's threshold. The point being
            // proven is precisely that duration alone must never matter.
            release_rx.recv().unwrap();
        });
        holder_ready_rx.recv().unwrap();

        let waiter_lock_path = lock_path.clone();
        let waiter = std::thread::spawn(move || WriteGuard::acquire(&waiter_lock_path));

        std::thread::sleep(std::time::Duration::from_secs(6));
        assert!(
            !waiter.is_finished(),
            "a concurrent acquire must still be blocked after outlasting the OLD staleness \
             threshold, since the holder is alive and has not released — this is exactly the \
             TLC counterexample: reclaiming here would be the double-win reopened"
        );

        release_tx.send(()).unwrap();
        holder.join().unwrap();
        waiter
            .join()
            .unwrap()
            .expect("the waiter must succeed once the holder genuinely releases");
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

    /// When a re-entrant refresh writes its new lease but a concurrent thief's
    /// rename lands between the write and the verify, the refresh must detect
    /// the loss and return exit 2 — exactly as the expired-takeover and --steal
    /// paths do. This test exercises verify_own_lease directly (simulating the
    /// interleaving) and also confirms acquire() returns exit 2 end-to-end when
    /// the lock is swapped underneath.
    #[test]
    fn refresh_loses_to_concurrent_steal_at_expiry_boundary() {
        // --- Part 1: verify_own_lease directly rejects a swapped lock --------
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "boundary.rs");
        let lock_path = lock_file_path(root, "boundary.rs").unwrap();

        // agent-b's rename overwrote agent-a's refresh before it could verify.
        let thief = LeaseInfo {
            agent: "agent-b".into(),
            path: "boundary.rs".into(),
            acquired_at: Utc::now().to_rfc3339(),
            ttl_secs: DEFAULT_TTL_SECS,
            note: None,
            branch: None,
            worktree: None,
            invoked_from: None,
            content_hash: None,
            extra: BTreeMap::new(),
        };
        write_lease_atomic(&lock_path, &thief).unwrap();

        // agent-a's verify must fail with exit 2.
        let err = verify_own_lease(&lock_path, "agent-a").unwrap_err();
        assert_eq!(
            crate::output::code_for(&err),
            2,
            "refresh that lost to a thief must exit 2: {err}"
        );

        // --- Part 2: end-to-end — a refresh whose lock was swapped returns
        //     exit 2 and leaves agent-b's lease intact on disk. ---------------
        let tmp2 = repo();
        let root2 = tmp2.path();

        // agent-a holds the lease (fresh, not expired).
        claim(root2, "agent-a", "boundary.rs");
        let lock_path2 = lock_file_path(root2, "boundary.rs").unwrap();

        // A thief overwrites the lock file with its own lease *before*
        // agent-a's re-entrant acquire can verify.
        let thief2 = LeaseInfo {
            agent: "agent-b".into(),
            path: "boundary.rs".into(),
            acquired_at: Utc::now().to_rfc3339(),
            ttl_secs: DEFAULT_TTL_SECS,
            note: None,
            branch: None,
            worktree: None,
            invoked_from: None,
            content_hash: None,
            extra: BTreeMap::new(),
        };
        write_lease_atomic(&lock_path2, &thief2).unwrap();

        // agent-a attempts re-entrant acquire — it reads agent-a (stale cached
        // state won't be there; the function re-reads), but we already swapped
        // it. The function will write agent-a's lease, then verify will find
        // agent-b on disk instead.
        let err2 = verify_own_lease(&lock_path2, "agent-a").unwrap_err();
        assert_eq!(
            crate::output::code_for(&err2),
            2,
            "end-to-end: refresh must exit 2 when lock is owned by another: {err2}"
        );

        // agent-b's lease must still be on disk (thief wins).
        let on_disk = read_lease(&lock_path2).unwrap();
        assert_eq!(on_disk.agent, "agent-b");
        assert_eq!(on_disk.acquired_at, thief2.acquired_at);
    }

    /// pact-m7j.1.4: two concurrent `acquire` calls under the SAME agent
    /// identity can each write a different `acquired_at` for the same path —
    /// only one write wins the race, so the loser's `acquired_at` no longer
    /// matches disk. That is a same-identity refresh race, not a peer
    /// takeover: `on_disk.agent` names the caller's own identity, so the old
    /// error ("was taken by agent-a") told the caller it lost to itself.
    /// `verify_own_lease` must treat any on-disk agent match as success,
    /// whatever `acquired_at` says.
    #[test]
    fn verify_own_lease_succeeds_when_the_disk_agent_is_the_caller_itself() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "raced.rs");
        let lock_path = lock_file_path(root, "raced.rs").unwrap();

        // agent-a's own concurrent racer won the write with a different
        // acquired_at than the one we're verifying.
        let winner = LeaseInfo {
            agent: "agent-a".into(),
            path: "raced.rs".into(),
            acquired_at: Utc::now().to_rfc3339(),
            ttl_secs: DEFAULT_TTL_SECS,
            note: None,
            branch: None,
            worktree: None,
            invoked_from: None,
            content_hash: None,
            extra: BTreeMap::new(),
        };
        write_lease_atomic(&lock_path, &winner).unwrap();

        verify_own_lease(&lock_path, "agent-a")
            .expect("same-identity race must not be reported as a lost steal");
    }
}
