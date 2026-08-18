//! Taking, keeping and taking over a lease — `acquire`, its expired-takeover
//! and `--steal` paths, `verify_own_lease`, and `renew`.
//!
//! **These four share ONE correctness argument and must stay in one file.**
//! The argument is about a single race, and it reads as one thing or not at
//! all:
//!
//! Every path that ends in "this agent now holds the lock" is a
//! read-decide-write sequence over the same lock file — read `existing`,
//! decide (first claim / expired reclaim / re-entrant refresh / `--steal`),
//! write. Without mutual exclusion, two racers can each read the same
//! `existing`, each decide they are entitled to it, each write, and each
//! re-read their own name: **both get `Ok`**. That is not hypothetical. It was
//! reproduced against the compiled binary through ordinary CLI-level races —
//! double-wins in 20-30% of rounds at N=6..10 on one pre-expired lock, and 2
//! of 30 rounds at plain N=2 (pact-iup, pact-ehi).
//!
//! [`WriteGuard`] is what closes it: a real `flock(2)` making the read and the
//! write one atomic unit, so the second racer reads the first racer's fresh
//! write and decides against current reality. `verify_own_lease` stays after
//! every write as a cheap, independent check that the guard worked — the role
//! an assertion plays after a lock, not the primary defense. The guard's first
//! design used a marker file reclaimed on a staleness heuristic; TLA+ model
//! checking (pact-kqb) proved that unsound, because under genuine contention a
//! live holder can still be inside the critical section when the threshold
//! elapses. Only proof of the holder's death is sound, and only the kernel has
//! it.
//!
//! Split these into four files and you get four halves of that argument in
//! four places, which is how it stops being checkable. The file is over the
//! 800-line target for that reason and no other.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::events;
use crate::otel;
use crate::output::exit_with;
use crate::watch;

use super::*;

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
pub(super) struct WriteGuard {
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
    pub(super) fn acquire(lock_path: &Path) -> Result<Self> {
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
pub(super) fn acquire_fs(
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

pub(super) fn acquire_inner(
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

pub(super) fn acquire_many_fs(
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

/// Refresh `acquired_at` on a lease `agent` already holds, so a long task can
/// outlive its TTL on purpose instead of by accident (pact-rnc.9).
/// Deliberately does NOT create a missing lease: a typo'd path must not
/// silently claim something new.
pub fn renew(repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo> {
    current_store().renew(repo_root, agent, path)
}

pub(super) fn renew_fs(repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::testutil::*;
    use chrono::Duration;

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
