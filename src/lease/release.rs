//! Giving a lease back: `release`, `release --all`, and the sweep that
//! collects locks whose holders let them lapse.
//!
//! The invariant here is that a release must say what it actually did.
//! Until pact-mqw.7 three different outcomes printed the same line and exited
//! 0 — a real release, an idempotent no-op, and a lease that had already
//! expired out from under its holder. None of them is an error, but they
//! differ in what they tell an agent about its own conduct, and
//! commit-before-release is the one rule the event log exists to prove.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;

use crate::events;
use crate::git_history;
use crate::otel;
use crate::output::exit_with;
use crate::watch;

use super::*;

/// Release a lease. See [`ReleaseOutcome`] for why the four cases are told apart
/// rather than all reported as success.
pub fn release(repo_root: &Path, agent: &str, path: &str, force: bool) -> Result<ReleaseOutcome> {
    current_store().release(repo_root, agent, path, force)
}

pub(super) fn release_fs(
    repo_root: &Path,
    agent: &str,
    path: &str,
    force: bool,
) -> Result<ReleaseOutcome> {
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
pub(super) fn collect_expired(repo_root: &Path, lock_path: &Path, lease: &LeaseInfo) {
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
            // The holder's own recorded context — where they were, on what branch, in
            // what worktree, under what harness — so this row says something true
            // about the lease it closes. A lock written before pact recorded a given
            // field leaves that field `None`, which correctly falls back to the
            // sweeper's rather than inventing one.
            Some(lease),
        );
    }
}

/// Has this holder COMMITTED under the path it holds, since it took it?
///
/// The one signal pact has that distinguishes a working agent from an absent one,
/// and it exists because every other signal pact has is a mutation of pact's own
/// state. An agent doing one deep change to one file emits no lease, message or
/// context row between acquire and release, so sustained work and abandonment
/// produce an identical silence — measured over this repository's history, 23% of
/// all 335 completed holds ran longer than half their TTL, which is nearly a
/// quarter of ordinary work looking abandoned (pact-g50).
///
/// Shared rather than duplicated, so `lease sweep --suspect` and `pact doctor`
/// cannot disagree about whether a quiet holder is working (pact-x16.7). They
/// used to: the sweep spared such a holder while every read-only surface still
/// called it SUSPECT. `lease ls` and the TUI still do, and deliberately — this
/// question costs a `git log`, and those two run on the dashboard's refresh path.
/// An operator command can afford it; a listing cannot.
///
/// Best-effort by signature, like every other git read on this path: no git, an
/// unparsable timestamp, a bare repo — all yield no commits and every caller
/// proceeds exactly as it did before this existed.
pub(crate) fn has_committed_under(commits: &[git_history::Commit], lease: &LeaseInfo) -> bool {
    let Some(acquired) = chrono::DateTime::parse_from_rfc3339(&lease.acquired_at)
        .ok()
        .map(|t| t.with_timezone(&Utc))
    else {
        return false;
    };
    commits
        .iter()
        .any(|c| c.at > acquired && c.paths.iter().any(|p| p == &lease.path))
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
    // Populated on first use; see the working-holder rescue below.
    let mut commits: Option<Vec<git_history::Commit>> = None;
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

        let mut eligible = entry.expired || (mode == Sweep::Suspect && entry.suspect);
        let past_ttl = entry
            .expired
            .then(|| -entry.remaining_secs)
            .filter(|s| *s > 0);

        // THE SIGNAL LADDER, cheapest and strongest first (pact-88z / pact-g50).
        //
        //   silence          the holder wrote no EVENT for over half its TTL.
        //                    This is what `suspect` means, and on its own it is
        //                    weak: pact only sees an agent when it MUTATES
        //                    something, so a worker making one deep change emits
        //                    nothing between acquire and release.
        //   commit           the holder has committed under this path since
        //                    taking it. Strong, and expensive: one `git log`.
        //   pact activity    the holder has run ANY pact command recently —
        //                    including the read-only ones that write no event.
        //
        // Activity is checked FIRST, and it is both the strongest rung and the
        // cheapest: one small file read against a `git log`. It also closes what
        // the commit rung could not. An agent that ran `pact msg inbox` two
        // minutes ago is alive whether or not it has committed anything — and
        // reading the inbox is exactly what a deep-change worker does most, while
        // being the one thing that used to leave no trace at all.
        //
        // NOT IMPLEMENTED, and named rather than quietly skipped: a working-tree
        // mtime rung between commit and activity — "has this path changed on
        // disk since the hold opened". It would catch the worker who has neither
        // committed nor run a pact command, which activity mostly subsumes. It is
        // absent because a `stat` per swept path is affordable but the semantics
        // are not: a worktree fleet edits its own checkout, so the path this
        // process can stat is frequently not the one the holder is editing, and a
        // rung that is silently wrong under the topology pact recommends is worse
        // than a rung that is missing.
        // The activity rung is not applied HERE, and its absence is the design
        // rather than an omission: `entry.suspect` already accounts for it.
        // `scan` folds pact activity into `holder_silent_secs` alongside the
        // event log, so a holder that has run any pact command inside half its
        // TTL is not suspect in the first place and never reaches this loop.
        //
        // One source of truth, deliberately. Checking it again here would be
        // dead code that reads as a safety net, and the two copies would drift
        // the first time either threshold moved.
        //
        // A holder that has COMMITTED under the path it holds is working,
        // whatever the event log says (pact-g50).
        //
        // `suspect` means "no pact event from this agent for over half its
        // TTL", and pact only ever sees an agent when it mutates something —
        // a lease, a message, a context row. An agent doing one deep change to
        // one file emits none of those between acquire and release, so
        // sustained work and abandonment produce an identical signal. Measured
        // over pact's own history: 23% of all 335 completed holds ran longer
        // than half their TTL, so nearly a quarter of ordinary work is
        // eligible here on silence alone.
        //
        // Expiry is deliberately NOT second-guessed: a lapsed hold is nobody's
        // by its own terms, and that is a statement about the clock rather than
        // about the holder. This only rescues holds still inside their TTL.
        if eligible && !entry.expired {
            if commits.is_none() {
                // Once per sweep, not once per lease: `commits_since` is a
                // single `git log` however many paths are asked about. Deferred
                // to here so the default `Sweep::Expired` never spawns git at
                // all, and so `lease ls` — which shares `scan` and runs on the
                // TUI's refresh — is untouched.
                commits = Some(git_history::commits_since(repo_root, None).unwrap_or_default());
            }
            if has_committed_under(commits.as_deref().unwrap_or_default(), lease) {
                eligible = false;
            }
        }

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
        //
        // So `None`, and this is a CORRECTION (pact-c3y). This call used to pass
        // the holder's `invoked_from` while naming the sweeper as `agent`, which
        // produced a row attributing the sweeper's deliberate act to the holder's
        // location — the same shape of falsehood pact-83r.3 fixed for `expired`,
        // pointing the other way. Widening the parameter to the whole lock made
        // it visible: carrying the holder's branch, worktree, harness and model
        // onto a row whose `agent` is the sweeper would have said the sweeper was
        // running the holder's model. Who it was reclaimed FROM is in `detail`
        // and in nothing else that could be misread as the actor.
        log_event(
            repo_root,
            agent,
            "reclaimed",
            &lease.path,
            Some(format!("reclaimed from {}: {evidence}", lease.agent)),
            lease.ttl_secs,
            None,
            None,
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

pub(super) fn release_all_fs(repo_root: &Path, agent: &str) -> Result<Vec<String>> {
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

    /// pact-88z: activity outranks the commit rung, and closes what it could not.
    ///
    /// The shape the commit rescue cannot see: an agent holding a lease, silent
    /// in the EVENT log past half its TTL, with nothing committed — but running
    /// pact commands. Reading an inbox writes no event, so before this it was
    /// indistinguishable from an agent that had stopped, and `--suspect` deleted
    /// its lock.
    ///
    /// Two holders, identical in every respect the sweep could previously see.
    /// Only one has participated.
    #[test]
    fn sweep_suspect_spares_a_holder_that_has_run_a_pact_command() {
        let tmp = repo();
        let root = tmp.path();
        // Quiet in the log past half their TTL: both are `suspect`.
        claim_aged(root, "reader", "reads.rs", 600, 400);
        claim_aged(root, "gone", "abandoned.rs", 600, 400);
        assert!(!lock_exists(root, "nothing"), "fixture sanity");

        // `reader` participated — read-only, so it wrote no event and the log
        // still shows it silent. That is the entire point.
        crate::activity::touch(&crate::repo::pact_dir_path(root), "reader");

        sweep(root, "sweeper", Sweep::Suspect, &[]).unwrap();

        assert!(
            lock_exists(root, "reads.rs"),
            "an agent that ran a pact command is alive, commits or not"
        );
        assert!(
            !lock_exists(root, "abandoned.rs"),
            "and a genuinely silent holder is still reclaimed — a rescue that \
             spares everyone is a disabled check"
        );
    }

    /// Stale activity does not rescue: the record is a timestamp, not a flag.
    ///
    /// Judged against the lease's OWN ttl, the same halving that made the holder
    /// suspect in the first place — so the two cannot disagree about where the
    /// line is.
    #[test]
    fn activity_older_than_half_the_ttl_does_not_rescue() {
        let tmp = repo();
        let root = tmp.path();
        claim_aged(root, "parked", "f.rs", 600, 400);

        // Seen, but longer ago than half of its 600s ttl.
        let stale = (Utc::now() - chrono::Duration::seconds(400)).to_rfc3339();
        let dir = crate::repo::pact_dir_path(root).join("activity");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("parked"), stale).unwrap();

        sweep(root, "sweeper", Sweep::Suspect, &[]).unwrap();

        assert!(
            !lock_exists(root, "f.rs"),
            "an agent last seen 400s into a 600s lease is as silent as one never seen"
        );
    }

    /// A holder that has committed under its path is working, and must survive
    /// a `--suspect` sweep that reclaims its silent neighbour (pact-g50).
    ///
    /// Both holds in this test are identically suspect: same agent, same TTL,
    /// same silence, same absence of any event since acquire. The ONLY
    /// difference is that one path has a commit after the lease was taken. If
    /// the rescue were reading anything else — git being present, the sweep
    /// bailing out wholesale — both would survive, and the assertion that the
    /// neighbour IS reclaimed is what rules that out.
    ///
    /// Why this matters at all: `suspect` fires on silence past half the TTL,
    /// and pact only sees an agent when it mutates something. Over pact's own
    /// history 23% of all 335 completed holds ran longer than that, so on
    /// silence alone this sweep reclaims roughly a quarter of ordinary work
    /// out from under agents that are still in it.
    #[test]
    fn sweep_suspect_spares_a_silent_holder_that_has_committed_under_its_path() {
        let tmp = repo();
        let root = tmp.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed");
        };
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: no git");
            return;
        }
        git(&["init", "-q"]);
        git(&["config", "user.email", "tests@pact.invalid"]);
        git(&["config", "user.name", "pact tests"]);

        let ttl = 600u64;
        let long_ago = Utc::now() - Duration::seconds(400);
        for path in ["printer.rs", "silent.rs"] {
            claim_at(root, "deep-worker", path, ttl, long_ago);
            events::append(
                root,
                &events::Event {
                    at: long_ago.to_rfc3339(),
                    agent: "deep-worker".to_string(),
                    kind: "acquired".to_string(),
                    path: Some(path.to_string()),
                    ttl_secs: Some(ttl),
                    ..blank_event()
                },
            );
        }

        // The commit lands AFTER both acquires, and touches only one of them.
        std::fs::write(root.join("printer.rs"), "fn main() {}\n").unwrap();
        git(&["add", "printer.rs"]);
        git(&["commit", "-q", "-m", "work under the held path"]);

        let swept = sweep(root, "rescuer", Sweep::Suspect, &[]).unwrap();
        let by = |p: &str| {
            swept
                .iter()
                .find(|s| s.path == p)
                .unwrap_or_else(|| panic!("{p} missing from {swept:?}"))
        };

        assert!(
            !by("printer.rs").reclaimed,
            "a holder committing under its own path is working: {swept:?}"
        );
        assert!(
            by("silent.rs").reclaimed,
            "the identically-silent path with no commit must still be reclaimed, \
             or this test proves nothing about WHY the other survived: {swept:?}"
        );
        assert!(root.join(".pact/leases").exists());
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
        // Same sentinels for the rest of the holder's context (pact-c3y). Every
        // one of these is a fact about the LEASE, and every one of them would
        // otherwise be filled from whichever process happened to sweep the lock —
        // `branch` in particular, because the sweeper is usually in the main
        // checkout on a different branch entirely.
        lapsed.branch = Some("holders-branch".to_string());
        lapsed.worktree = Some("holders-worktree".to_string());
        lapsed.harness = Some("holders-harness".to_string());
        lapsed.model = Some("holders-model".to_string());
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
        // The rest of the chain, same rule (pact-c3y). These are load-bearing in a
        // way the assertion above is not: `invoked_from` had a hand-written
        // holder-wins branch, while these four ride the general "a caller that
        // already knows wins" rule in `stamp_context`. If that rule is ever
        // simplified back to unconditional assignment, this is what notices —
        // and it would notice quietly nowhere else, because in a single-checkout
        // test repo the sweeper's real branch and the holder's are the same
        // string.
        assert_eq!(expired.branch.as_deref(), Some("holders-branch"));
        assert_eq!(expired.worktree.as_deref(), Some("holders-worktree"));
        assert_eq!(expired.harness.as_deref(), Some("holders-harness"));
        assert_eq!(expired.model.as_deref(), Some("holders-model"));
    }

    /// A reclaim is the SWEEPER's act, so it carries the SWEEPER's context —
    /// the mirror image of the test above, and a correction (pact-c3y).
    ///
    /// `sweep --suspect` used to pass the holder's `invoked_from` onto a row whose
    /// `agent` is the sweeper, which is the pact-83r.3 falsehood pointing the
    /// other way: a deliberate act attributed to somebody else's location. Adding
    /// harness and model made it untenable rather than merely odd — that row would
    /// have said the sweeper was running the holder's model.
    ///
    /// Who it was reclaimed FROM survives in `detail`, which is the one place it
    /// cannot be mistaken for the actor.
    #[test]
    fn a_reclaim_carries_the_sweepers_context_not_the_holders() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "f.rs");

        let path = lock_file_path(root, "f.rs").unwrap();
        let mut lock = read_lease(&path).unwrap();
        lock.invoked_from = Some("holders-worktree".to_string());
        lock.branch = Some("holders-branch".to_string());
        lock.harness = Some("holders-harness".to_string());
        lock.model = Some("holders-model".to_string());
        // Lapsed far enough that the sweep is unambiguous.
        lock.acquired_at =
            (Utc::now() - chrono::Duration::seconds(lock.ttl_secs as i64 * 4)).to_rfc3339();
        write_lease_atomic(&path, &lock).unwrap();

        sweep(root, "sweeper", Sweep::Expired, &[]).unwrap();

        let reclaimed = crate::events::recent(root, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "reclaimed");
        if let Some(e) = reclaimed {
            assert_eq!(e.agent, "sweeper");
            assert_ne!(
                e.invoked_from.as_deref(),
                Some("holders-worktree"),
                "a reclaim must not claim the holder's location for the sweeper"
            );
            assert_ne!(e.harness.as_deref(), Some("holders-harness"));
            assert_ne!(e.model.as_deref(), Some("holders-model"));
            assert!(
                e.detail.as_deref().unwrap_or_default().contains("agent-a"),
                "who it was reclaimed from must survive, in detail: {:?}",
                e.detail
            );
        }
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
}
