//! Where a lease lives on disk: the [`LeaseStore`] indirection, the
//! [`FileLeaseStore`] that backs every command, and the primitives underneath
//! it — lock-file locations, atomic write and read, the clock watermark, and
//! the scanners that answer questions about the lock directory.
//!
//! The read-only/mutating split is load-bearing and is enforced here rather
//! than by convention: `peek` and `effective_now_readonly` must leave nothing
//! behind, because a question must not mutate (pact-rnc.19, pact-rnc.27),
//! while `list` sweeps expired locks because that is `lease ls`'s documented
//! job.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::events;
use crate::repo::pact_dir;

use super::*;

/// Storage backend for lease operations.
pub trait LeaseStore {
    fn acquire(
        &self,
        repo_root: &Path,
        agent: &str,
        path: &str,
        ttl_secs: u64,
        steal: bool,
        note: Option<String>,
    ) -> Result<AcquireOutcome>;
    fn acquire_many(
        &self,
        repo_root: &Path,
        agent: &str,
        paths: &[String],
        ttl_secs: u64,
        steal: bool,
        note: Option<String>,
    ) -> Result<Vec<AcquireOutcome>>;
    fn release(
        &self,
        repo_root: &Path,
        agent: &str,
        path: &str,
        force: bool,
    ) -> Result<ReleaseOutcome>;
    fn release_all(&self, repo_root: &Path, agent: &str) -> Result<Vec<String>>;
    fn renew(&self, repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo>;
    fn list_reclaiming(
        &self,
        repo_root: &Path,
        all: bool,
    ) -> Result<(Vec<LeaseEntry>, Vec<LeaseEntry>)>;
    fn peek(&self, repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FileLeaseStore;

impl LeaseStore for FileLeaseStore {
    fn acquire(
        &self,
        repo_root: &Path,
        agent: &str,
        path: &str,
        ttl_secs: u64,
        steal: bool,
        note: Option<String>,
    ) -> Result<AcquireOutcome> {
        acquire_fs(repo_root, agent, path, ttl_secs, steal, note)
    }

    fn acquire_many(
        &self,
        repo_root: &Path,
        agent: &str,
        paths: &[String],
        ttl_secs: u64,
        steal: bool,
        note: Option<String>,
    ) -> Result<Vec<AcquireOutcome>> {
        acquire_many_fs(repo_root, agent, paths, ttl_secs, steal, note)
    }

    fn release(
        &self,
        repo_root: &Path,
        agent: &str,
        path: &str,
        force: bool,
    ) -> Result<ReleaseOutcome> {
        release_fs(repo_root, agent, path, force)
    }

    fn release_all(&self, repo_root: &Path, agent: &str) -> Result<Vec<String>> {
        release_all_fs(repo_root, agent)
    }

    fn renew(&self, repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo> {
        renew_fs(repo_root, agent, path)
    }

    fn list_reclaiming(
        &self,
        repo_root: &Path,
        all: bool,
    ) -> Result<(Vec<LeaseEntry>, Vec<LeaseEntry>)> {
        list_reclaiming_fs(repo_root, all)
    }

    fn peek(&self, repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
        peek_fs(repo_root, all)
    }
}

static FILE_LEASE_STORE: FileLeaseStore = FileLeaseStore;

pub fn current_store() -> &'static dyn LeaseStore {
    &FILE_LEASE_STORE
}

pub(super) fn lock_file_path(repo_root: &Path, relative: &str) -> Result<PathBuf> {
    let dir = pact_dir(repo_root)?;
    Ok(dir
        .join("leases")
        .join(format!("{}.lock", encode_path(relative))))
}

/// The other well-known lock-file location a lease might actually be sitting
/// in, when a lookup at the normally-resolved directory misses (pact-m7j.9.6).
///
/// `RepoContext::resolve` has no cross-invocation memory: nothing detects that
/// `PACT_WORKTREE_SCOPE` or this repository's worktree topology differ now
/// from what they were when the lease was acquired, so a miss here is
/// consistent with either "genuinely nothing to release" or "it's in the
/// other directory a different scope/topology would have produced" — and
/// those look identical from a bare `!lock_path.exists()`.
///
/// Only two shapes are worth probing: `RepoContext::resolve_topology` ignores
/// both env vars and always lands on the topology's own answer (shared root
/// for a linked worktree, `worktree_root` otherwise), and
/// `PACT_WORKTREE_SCOPE=local` always redirects to `worktree_root.join(".pact")`
/// regardless of topology. Those are the only two directories scope/topology
/// drift between one pact invocation and the next can produce. An arbitrary
/// `PACT_STATE_DIR` value is a third, unrelated drift mechanism deliberately
/// left out: probing it would mean guessing at an operator-chosen path with no
/// finite candidate set, a bigger question the bead this fixes calls out as
/// needing a human decision, not a cheap probe. Skipped whenever
/// `PACT_STATE_DIR` is set, so a deliberate override (repo.rs's own test
/// isolation depends on one) never trips a false-positive warning.
pub(super) fn other_candidate_lock_path(repo_root: &Path, relative: &str) -> Option<PathBuf> {
    if std::env::var_os("PACT_STATE_DIR").is_some() {
        return None;
    }
    let resolved = crate::repo::pact_dir_path(repo_root);
    let local_shape = repo_root.join(".pact");
    let shared_shape = crate::repo::RepoContext::resolve_topology(repo_root).state_dir;
    let other = if resolved == local_shape {
        shared_shape
    } else {
        local_shape
    };
    (other != resolved).then(|| {
        other
            .join("leases")
            .join(format!("{}.lock", encode_path(relative)))
    })
}

/// `.pact/clock_watermark`: a single RFC3339 timestamp recording the highest
/// wall-clock `now` any pact command in this repo has ever observed.
///
/// This is the backward-jump counterpart to [`MAX_PLAUSIBLE_AGE_SECS`]
/// (pact-m7j.4.5): that constant keeps a *forward* clock jump from
/// auto-expiring a lease that is actually still live; this watermark keeps a
/// *backward* jump from resurrecting a lease that had already genuinely
/// expired under a `now` we already saw. `is_expired`/`age_and_remaining`
/// stay pure functions of `(lease, now)` — the correction lives entirely in
/// what callers pass as `now`, computed via [`effective_now`] or
/// [`effective_now_readonly`] instead of raw `Utc::now()`.
fn watermark_file(repo_root: &Path) -> PathBuf {
    crate::repo::pact_dir_path(repo_root).join("clock_watermark")
}

fn read_watermark(repo_root: &Path) -> Option<DateTime<Utc>> {
    let contents = std::fs::read_to_string(watermark_file(repo_root)).ok()?;
    DateTime::parse_from_rfc3339(contents.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// The wall clock, corrected against the persisted watermark but never
/// persisting a new one — safe to call from a read-only path like `peek`,
/// which must not leave `.pact/` behind on a mere question (pact-rnc.19,
/// pact-rnc.27).
pub(super) fn effective_now_readonly(repo_root: &Path) -> DateTime<Utc> {
    let raw = Utc::now();
    match read_watermark(repo_root) {
        Some(watermark) if watermark > raw => watermark,
        _ => raw,
    }
}

/// Same correction as [`effective_now_readonly`], and also bumps the
/// persisted watermark when `raw` is a new high point. Only call this from a
/// path that already mutates `.pact/` (acquire, renew, `lease ls`'s sweep) —
/// never from a read-only path.
pub(super) fn effective_now(repo_root: &Path) -> DateTime<Utc> {
    let raw = Utc::now();
    match read_watermark(repo_root) {
        Some(watermark) if watermark >= raw => watermark,
        _ => {
            // `raw` is a new high point (or none was ever recorded): persist
            // it. Best-effort: a lost write only costs one extra operation
            // before the next backward jump self-corrects, and a logging-style
            // failure here must never break the lease operation that
            // triggered it.
            if let Ok(dir) = pact_dir(repo_root) {
                let tmp = dir.join(crate::events::unique_temp_name("clock_watermark"));
                if std::fs::write(&tmp, raw.to_rfc3339()).is_ok() {
                    let _ = std::fs::rename(&tmp, dir.join("clock_watermark"));
                }
            }
            raw
        }
    }
}

/// Write `lease` to `lock_path` atomically: write to a sibling temp file, then
/// rename over the destination (rename is atomic on the same filesystem).
pub(super) fn write_lease_atomic(lock_path: &Path, lease: &LeaseInfo) -> Result<()> {
    let dir = lock_path
        .parent()
        .context("lock path unexpectedly has no parent")?;
    let tmp_path = dir.join(crate::events::unique_temp_name("tmp"));
    let json = serde_json::to_string_pretty(lease)?;
    std::fs::write(&tmp_path, json).with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, lock_path)
        .with_context(|| format!("renaming into {}", lock_path.display()))?;
    Ok(())
}

/// A staging file beside `lock_path`, unique per process AND per thread.
pub(super) fn temp_sibling(lock_path: &Path) -> PathBuf {
    let dir = lock_path.parent().unwrap_or(Path::new("."));
    dir.join(crate::events::unique_temp_name("staging"))
}

pub(super) fn read_lease(lock_path: &Path) -> Result<LeaseInfo> {
    let contents = std::fs::read_to_string(lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parsing lease at {}", lock_path.display()))
}

/// Read every lock file, paired with the file it came from. The single reader
/// behind both [`list`] and [`peek`], so the two cannot drift apart: they
/// differ in exactly one thing (whether expired locks are unlinked) and share
/// everything else — parsing, skipping, age and state computation.
pub(super) fn scan(repo_root: &Path) -> Result<Vec<(PathBuf, LeaseEntry)>> {
    // Non-creating path: listing leases is a question, and a question must not
    // leave a `.pact/` behind in a repo that has never used pact (pact-rnc.27).
    let leases_dir = crate::repo::pact_dir_path(repo_root).join("leases");
    // Read-only correction (pact-m7j.4.5): `scan` backs `peek`, which must not
    // mutate `.pact/` on a mere question, so this must not persist a new
    // watermark — see `effective_now_readonly`'s doc comment. `list` still
    // benefits: the watermark it reads was written by whatever `acquire`/
    // `renew` call created the lease it is now sweeping.
    let now = effective_now_readonly(repo_root);
    // ONE log read for the whole listing, not one per lease. `actors` already
    // returns every agent's most recent event, which is exactly the question.
    // Best-effort: an unreadable or absent log means "no liveness signal", never
    // a failed listing — `lease ls` must keep working in a repo whose log was
    // never committed.
    let last_seen: Vec<(String, String, usize)> = events::actors(repo_root).unwrap_or_default();
    // And ONE directory read for the activity records, for the same reason
    // (pact-88z). `actors` answers "when did this agent last MUTATE something";
    // this answers "when did it last run anything at all", which is a strictly
    // wider question and the one `suspect` was always trying to ask.
    //
    // Affordable here where the commit rung is not: this is a `read_dir` of a
    // directory holding one small file per agent — measured at 14.7 µs to WRITE
    // one, less to read — against the `git log` that made `has_committed_under`
    // sweep-only. `scan` backs `lease ls` and the TUI's refresh, so the
    // distinction between a file read and a subprocess is the whole difference
    // between a signal this surface can carry and one it cannot.
    let last_active: std::collections::BTreeMap<String, chrono::DateTime<Utc>> =
        crate::activity::all(&crate::repo::pact_dir_path(repo_root))
            .into_iter()
            .collect();
    let mut entries = Vec::new();

    let dir = match std::fs::read_dir(&leases_dir) {
        Ok(d) => d,
        // No directory yet simply means no leases have ever been taken.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(e) => return Err(e).with_context(|| format!("reading {}", leases_dir.display())),
    };

    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue;
        }
        // Advisory tooling, not a database: garbage on disk (partial writes,
        // hand-edited files) is skipped rather than treated as fatal.
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(lease) = serde_json::from_str::<LeaseInfo>(&contents) else {
            continue;
        };

        let (age_secs, remaining_secs) = age_and_remaining(&lease, now);
        let expired = is_expired(&lease, now);
        // "Silent" now means silent in EVERY channel pact can see, which is what
        // the word was always meant to mean (pact-88z). Before this it meant
        // "wrote no event", and pact only writes events for mutations — so an
        // agent reading its inbox, listing leases or reading the log was as
        // silent as one that had stopped existing. That is the pact-g50 residual,
        // and it made SUSPECT fire on roughly a quarter of ordinary work.
        //
        // The MORE RECENT of the two wins, never the average and never the event
        // alone: each is evidence the agent was alive at that moment, and the
        // question is how long ago the last such moment was.
        let event_silence = last_seen
            .iter()
            .find(|(agent, _, _)| *agent == lease.agent)
            .and_then(|(_, at, _)| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| (now - at.with_timezone(&Utc)).num_seconds().max(0));
        let activity_silence = last_active
            .get(&lease.agent)
            .map(|at| (now - *at).num_seconds().max(0));
        let holder_silent_secs = match (event_silence, activity_silence) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (only, None) | (None, only) => only,
        };
        // Half the lease's OWN ttl, not a global constant: a 10-minute lease and a
        // 45-minute one deserve different patience, and the lease already carries
        // the number. An already-expired lease is not flagged — it has a louder
        // label of its own, and calling it suspect too would just double-report.
        let suspect = !expired
            && match holder_silent_secs {
                Some(silent) => silent * 2 > ttl_as_i64(lease.ttl_secs),
                // No event at all from an agent holding a lock: either the log was
                // never written (a hand-planted lock, a fresh clone) or it has been
                // rewritten past this agent's last line. Either way pact cannot
                // corroborate that anyone is behind this claim, which is precisely
                // what this column is for.
                None => true,
            };

        entries.push((
            path,
            LeaseEntry {
                lease,
                age_secs,
                remaining_secs,
                expired,
                holder_silent_secs,
                suspect,
            },
        ));
    }

    Ok(entries)
}

/// List active leases, garbage-collecting expired ones as a side effect.
/// `all` includes expired leases in the returned list (still GC'd from disk).
///
/// The sweep is documented behaviour for `pact lease ls` (docs/leases.md), so
/// it stays. Anything merely *asking* who holds what — `pact agents`, the TUI,
/// `msg send`'s recipient check — must use [`peek`] instead (pact-rnc.19).
/// Every lease on disk, plus the holds this call just reclaimed.
///
/// **`list` IS the garbage collector**, and until now it collected in silence: an
/// expired lock was unlinked and its row simply did not appear. From the CLI a
/// released hold and a dead one therefore looked identical — both are absent from
/// the next table — so a fleet losing agents reads as a fleet finishing work.
///
/// Measured in the modmill proving-ground run (pact-k1n.1), on a lease whose
/// holder had been killed mid-edit and which was then left alone for its full
/// 45-minute TTL: at the moment it lapsed, `pact lease ls` went straight from
/// showing the hold as `active` to "no active leases", with nothing in between
/// naming what had just happened. The operator had to know to reach for `--all`,
/// and by then this call had already collected the lock.
///
/// So the reclaimed set is returned rather than dropped. `list`'s own shape is
/// untouched — `lease ls --json` is a pinned array of `LeaseEntry` (pact-er0), and
/// a reclaimed hold no longer has a lock to describe — and the reporting is the
/// caller's to render.
fn list_reclaiming_fs(repo_root: &Path, all: bool) -> Result<(Vec<LeaseEntry>, Vec<LeaseEntry>)> {
    let mut entries = Vec::new();
    let mut reclaimed = Vec::new();
    for (lock_path, entry) in scan(repo_root)? {
        if entry.expired {
            collect_expired(repo_root, &lock_path, &entry.lease);
            // Recorded from the SAME scan that collected it, so there is no window
            // in which a lease could lapse between observing and reclaiming and go
            // unreported. A second pass with `peek` would have had exactly that
            // race, and would have under-reported precisely when the fleet was
            // busiest.
            reclaimed.push(entry.clone());
            if !all {
                continue;
            }
        }
        entries.push(entry);
    }
    Ok((entries, reclaimed))
}

/// Every lease, and the expired holds this call reclaimed on its way there.
///
/// **The GC and the listing are one call, and the reclaimed set is returned
/// rather than dropped.** There is no `list` that discards it: a second entry
/// point would let a caller collect in silence again, which is exactly the shape
/// pact-k1n.1 came back from the field to fix.
pub fn list_reclaiming(repo_root: &Path, all: bool) -> Result<(Vec<LeaseEntry>, Vec<LeaseEntry>)> {
    current_store().list_reclaiming(repo_root, all)
}

/// The non-mutating twin of [`list`]: same view, nothing deleted (pact-rnc.19).
///
/// Answering "who holds what" must not change the answer. With `list`, an agent
/// whose lease had just expired showed up in the first `pact agents` and was
/// gone from the second, because the first call unlinked the evidence — and two
/// concurrent readers raced on the same unlink. `peek` is safe to call
/// repeatedly and concurrently; GC belongs on `acquire` and on `lease ls`.
pub fn peek(repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
    current_store().peek(repo_root, all)
}

pub(super) fn peek_fs(repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
    Ok(scan(repo_root)?
        .into_iter()
        .map(|(_, entry)| entry)
        .filter(|entry| all || !entry.expired)
        .collect())
}

/// Count lock files under `.pact/leases/` whose JSON cannot be parsed.
///
/// These are unreadable by the normal scan and would otherwise be invisible to
/// the operator. A corrupted lock is not an active hold, but it is noise that
/// should be surfaced (e.g. by `pact doctor`).
pub fn corrupt_count(repo_root: &Path) -> Result<usize> {
    let leases_dir = crate::repo::pact_dir_path(repo_root).join("leases");
    let dir = match std::fs::read_dir(&leases_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", leases_dir.display())),
    };
    let mut count = 0;
    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue;
        }
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                count += 1;
                continue;
            }
        };
        if serde_json::from_str::<LeaseInfo>(&contents).is_err() {
            count += 1;
        }
    }
    Ok(count)
}

/// Count leftover `staging-*`/`tmp-*` files under `.pact/leases/`: the
/// sibling half of a crash `corrupt_count` cannot see.
///
/// `temp_sibling` and `write_lease_atomic` both stage their write beside the
/// `.lock` file they are about to create/replace, then rename it into place.
/// A crash between the write and the rename leaves the staging file behind
/// forever — nothing ever revisits it, because `scan()` and `corrupt_count`
/// both filter on `path.extension() == Some("lock")`, and a staging file has
/// no `.lock` extension by construction. Surfaced here so `pact doctor` can
/// say so instead of the directory silently accumulating debris.
pub fn orphan_temp_count(repo_root: &Path) -> Result<usize> {
    orphan_temp_files(repo_root).map(|f| f.len())
}

/// The staging debris itself, so counting it and clearing it cannot disagree.
///
/// `doctor` reports the count and `doctor --fix` removes the files; two separate
/// walks of `.pact/leases/` would drift the moment either grew a rule about what
/// counts as debris, and the failure mode of that drift is deleting a lock file.
pub fn orphan_temp_files(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let leases_dir = crate::repo::pact_dir_path(repo_root).join("leases");
    let dir = match std::fs::read_dir(&leases_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", leases_dir.display())),
    };
    let mut found = Vec::new();
    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("lock") {
            continue;
        }
        found.push(path);
    }
    // Deterministic, so a report and the removal that follows list the same
    // files in the same order.
    found.sort();
    Ok(found)
}

/// Count `.pact/waits/*.wait` markers ([`mark_conflict`]'s breadcrumbs) that
/// nothing has swept yet.
///
/// A marker is only ever collected two ways: the same agent re-acquiring the
/// same path, or that agent's own `release --all` (see [`sweep_wait_markers`]).
/// AGENTS.md tells a blocked agent to do neither — "message them and pick up
/// something else" — so a marker left by a conflict nobody retried survives
/// forever with nothing else to revisit it (pact-m7j.4.6). This is pure
/// counting, not a judgment: unlike `corrupt_count`, a nonzero result here is
/// normal fleet behaviour, not damage, so `pact doctor` reports the number and
/// stops there rather than inventing a ceiling nobody has asked for.
///
/// Read-only, like `corrupt_count`: `pact_dir_path` never creates `.pact/`, so
/// asking this question on a repo with no state yet costs nothing and leaves
/// nothing behind.
pub fn marker_count(repo_root: &Path) -> Result<usize> {
    let waits_dir = crate::repo::pact_dir_path(repo_root).join("waits");
    let dir = match std::fs::read_dir(&waits_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", waits_dir.display())),
    };
    Ok(dir
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("wait"))
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::testutil::*;
    use chrono::Duration;

    /// pact-mqw.6: a stalled holder is strictly worse than a crashed one, and the
    /// TTL is the slowest possible detector of it. Seven of ten crucible agents
    /// ended their turn early waiting on a poller that could not wake them, one
    /// while holding `src/printer.rs` — a lease `lease ls` called `active` with a
    /// live holder, for minutes.
    ///
    /// Every case is driven through the real `peek` over a real log, not through a
    /// hand-built `LeaseEntry`, because the derivation is the thing under test.
    #[test]
    fn lease_ls_flags_a_holder_that_has_gone_quiet_for_half_its_ttl() {
        let tmp = repo();
        let root = tmp.path();
        let suspect_of = |path: &str| -> (bool, Option<i64>) {
            let e = peek(root, true)
                .unwrap()
                .into_iter()
                .find(|e| e.lease.path == path)
                .unwrap_or_else(|| panic!("no entry for {path}"));
            (e.suspect, e.holder_silent_secs)
        };

        // 1. A holder that just acted. `acquire` itself is an event, so a fresh
        //    lease is never suspect — which is also the "ttl not yet past the
        //    threshold" case: silence is measured from the last event, so a young
        //    lease cannot be quiet for half its ttl.
        acquire(root, "busy", "fresh.rs", 600, false, None).unwrap();
        assert_eq!(suspect_of("fresh.rs"), (false, Some(0)));

        // 2. A holder whose last event is old relative to its OWN ttl. The lease
        //    is still well inside its ttl — this is the whole point: the TTL says
        //    nothing yet, and this does.
        let ttl = 600u64;
        claim_at(
            root,
            "stalled",
            "printer.rs",
            ttl,
            Utc::now() - Duration::seconds(400),
        );
        events::append(
            root,
            &events::Event {
                at: (Utc::now() - Duration::seconds(400)).to_rfc3339(),
                agent: "stalled".to_string(),
                kind: "acquired".to_string(),
                path: Some("printer.rs".to_string()),
                detail: None,
                ttl_secs: Some(ttl),
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
            },
        );
        let (suspect, silent) = suspect_of("printer.rs");
        assert!(suspect, "quiet 400s against a 600s ttl must be suspect");
        assert!(
            silent.unwrap_or(0) >= 399,
            "and must report the age: {silent:?}"
        );
        // Still `active` to the machine — a suspect lease is exactly as
        // unavailable to a peer as any other live one.
        let entry = peek(root, true)
            .unwrap()
            .into_iter()
            .find(|e| e.lease.path == "printer.rs")
            .unwrap();
        assert_eq!(entry.state(), "active");
        assert!(
            entry.state_label().contains("SUSPECT"),
            "{}",
            entry.state_label()
        );

        // 3. A lock whose holder the log has never seen act at all — a
        //    hand-planted lock, a lock file from a clone, or a log rewritten past
        //    that agent's last line. pact cannot corroborate anyone is behind it.
        claim_at(root, "ghost", "planted.rs", 600, Utc::now());
        assert_eq!(suspect_of("planted.rs"), (true, None));

        // 4. Half of a LONGER ttl is not yet suspicious. Same 400s of silence as
        //    case 2, against pact's default ttl instead of 600s: the threshold is
        //    the lease's own patience, not a global constant.
        claim_at(
            root,
            "thinker",
            "long.rs",
            DEFAULT_TTL_SECS,
            Utc::now() - Duration::seconds(400),
        );
        events::append(
            root,
            &events::Event {
                at: (Utc::now() - Duration::seconds(400)).to_rfc3339(),
                agent: "thinker".to_string(),
                kind: "acquired".to_string(),
                path: Some("long.rs".to_string()),
                detail: None,
                ttl_secs: Some(DEFAULT_TTL_SECS),
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
            },
        );
        assert!(
            !suspect_of("long.rs").0,
            "400s of silence against a 2700s ttl is a thinking agent, not a stalled one"
        );

        // 5. An EXPIRED lease is never flagged: it has a louder label of its own,
        //    and double-reporting it would just add noise to the one row a peer
        //    can already act on.
        claim_at(
            root,
            "dead",
            "gone.rs",
            60,
            Utc::now() - Duration::seconds(60 + GRACE_SECS + 10),
        );
        let expired = peek(root, true)
            .unwrap()
            .into_iter()
            .find(|e| e.lease.path == "gone.rs")
            .unwrap();
        assert!(expired.expired);
        assert!(!expired.suspect, "an expired lease is not merely suspect");
    }

    #[test]
    fn corrupt_count_detects_unreadable_lock_files() {
        let tmp = repo();
        let root = tmp.path();

        // No leases dir yet → 0 corrupt.
        assert_eq!(corrupt_count(root).unwrap(), 0);

        // A valid lease does not count as corrupt.
        claim(root, "agent-a", "good.rs");
        assert_eq!(corrupt_count(root).unwrap(), 0);

        // Write a lock file whose contents are not valid JSON.
        let leases_dir = crate::repo::pact_dir_path(root).join("leases");
        std::fs::write(leases_dir.join("bad__rs.lock"), b"not json at all").unwrap();
        assert_eq!(corrupt_count(root).unwrap(), 1);

        // Write a second corrupt lock file.
        std::fs::write(leases_dir.join("also__rs.lock"), b"{}").unwrap();
        assert_eq!(corrupt_count(root).unwrap(), 2);
    }

    /// pact-m7j.4.1: a crash between `temp_sibling`'s write and its rename into
    /// place leaves a `staging-*`/`tmp-*` file behind, invisible to `scan()` and
    /// `corrupt_count` alike because neither looks past the `.lock` extension.
    #[test]
    fn orphan_temp_count_detects_leftover_staging_and_tmp_files() {
        let tmp = repo();
        let root = tmp.path();

        // No leases dir yet -> 0 orphans.
        assert_eq!(orphan_temp_count(root).unwrap(), 0);

        // A real lease's `.lock` file is not an orphan.
        claim(root, "agent-a", "good.rs");
        assert_eq!(orphan_temp_count(root).unwrap(), 0);

        let leases_dir = crate::repo::pact_dir_path(root).join("leases");
        std::fs::write(leases_dir.join("staging-123-ThreadId(1)-999"), b"{}").unwrap();
        assert_eq!(orphan_temp_count(root).unwrap(), 1);

        std::fs::write(leases_dir.join("tmp-456-ThreadId(1)-111"), b"{}").unwrap();
        assert_eq!(orphan_temp_count(root).unwrap(), 2);
    }

    #[test]
    fn backward_clock_jump_does_not_resurrect_an_expired_lease() {
        // pact-m7j.4.5: a lease that is genuinely expired must not be seen as
        // live again just because the wall clock jumps backward. There is no
        // way to move the real system clock from a test, so this fabricates
        // a watermark *ahead* of the real clock instead — from the code's
        // point of view that is indistinguishable from "the real clock later
        // jumped behind a `now` some earlier pact command already observed",
        // which is exactly the scenario the persisted watermark exists for.
        let tmp = repo();
        let root = tmp.path();

        let watermark = Utc::now() + Duration::days(400);
        let ttl = 100u64;
        // Genuinely expired *relative to the watermark*: past ttl+grace as
        // measured from the already-observed point in time.
        let acquired_at = watermark - Duration::seconds(ttl as i64 + GRACE_SECS + 1);
        claim_at(root, "agent-a", "dead.rs", ttl, acquired_at);

        // Plant the watermark after the claim (claim_at's write_lease_atomic
        // already created `.pact/`).
        std::fs::write(watermark_file(root), watermark.to_rfc3339()).unwrap();

        // Confirm the fabricated setup really is a backward jump relative to
        // this lease: the raw wall clock right now is still *before*
        // `acquired_at`, so a naive `is_expired(&lease, Utc::now())` would
        // report it as not even acquired yet, let alone expired.
        assert!(
            Utc::now() < acquired_at,
            "test setup: the real clock must be behind the fabricated watermark"
        );

        let entries = peek(root, true).unwrap();
        let entry = entries
            .iter()
            .find(|e| e.lease.path == "dead.rs")
            .expect("dead.rs must still be visible to peek");
        assert!(
            entry.expired,
            "a lease already expired relative to the watermark must be reported expired \
             promptly via the watermark, not gated on the real clock re-advancing past the jump"
        );
    }

    #[test]
    fn listing_creates_nothing_on_a_repo_that_has_never_used_pact() {
        // pact-rnc.27: same principle as peek() not sweeping (pact-rnc.19) --
        // asking what is leased must not leave a `.pact/` behind, and a missing
        // directory means "nothing leased yet", not an error.
        let tmp = repo();
        let root = tmp.path();
        assert!(peek(root, true).unwrap().is_empty());
        assert!(list_reclaiming(root, true).unwrap().0.is_empty());
        assert!(
            !root.join(".pact").exists(),
            "listing leases created .pact/ on a repo that never used pact"
        );
    }

    #[test]
    fn peek_does_not_delete_an_expired_lock_but_list_does() {
        let tmp = repo();
        let root = tmp.path();
        claim_aged(root, "agent-a", "dead.rs", 100, 100 + GRACE_SECS + 1);
        claim(root, "agent-b", "live.rs");

        // Asking twice must give the same answer, and leave the evidence alone.
        for _ in 0..2 {
            let paths: Vec<String> = peek(root, true)
                .unwrap()
                .into_iter()
                .map(|e| e.lease.path)
                .collect();
            assert!(
                paths.contains(&"dead.rs".to_string()),
                "peek must keep reporting the expired lease: {paths:?}"
            );
            assert!(lock_exists(root, "dead.rs"), "peek must not unlink");
        }

        // Same view, minus expired, still without deleting.
        let active: Vec<String> = peek(root, false)
            .unwrap()
            .into_iter()
            .map(|e| e.lease.path)
            .collect();
        assert_eq!(active, vec!["live.rs".to_string()]);
        assert!(lock_exists(root, "dead.rs"));

        // list() keeps sweeping: documented behaviour for `pact lease ls`.
        let listed = list_reclaiming(root, true).unwrap().0;
        assert_eq!(listed.len(), 2, "--all still shows the swept lease once");
        assert!(!lock_exists(root, "dead.rs"), "list must GC");
        assert!(lock_exists(root, "live.rs"));
    }

    // ---- pact-rnc.21: atomic multi-path claims ---------------------------

    /// The read-only twin stays read-only: [`peek`] unlinks nothing, so it must
    /// not claim a lapse either (pact-rnc.19 + pact-rnc.13).
    #[test]
    fn peek_does_not_log_an_expiry() {
        let tmp = repo();
        let root = tmp.path();
        claim_aged(root, "agent-a", "dead.rs", 60, 60 + GRACE_SECS + 1);

        assert_eq!(peek(root, true).unwrap().len(), 1);
        assert!(crate::events::recent(root, 10).unwrap().is_empty());
    }

    /// Two agents' worth of conflicts, neither ever retried nor swept by
    /// `release --all` — the exact shape AGENTS.md's own protocol leaves
    /// behind ("message them and pick up something else"), and what `pact
    /// doctor`'s "stale wait markers" check exists to count (pact-m7j.4.6).
    #[cfg(feature = "otel")]
    #[test]
    fn marker_count_counts_conflicts_nobody_retried_or_swept() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "hot.rs");
        assert!(acquire(root, "agent-b", "hot.rs", 900, false, None).is_err());

        claim(root, "agent-x", "warm.rs");
        assert!(acquire(root, "agent-y", "warm.rs", 900, false, None).is_err());

        assert_eq!(marker_count(root).unwrap(), 2);
    }

    /// The markers live beside the locks and must stay invisible to everything
    /// that reads them — `scan` (so `lease ls` cannot report a wait as a
    /// lease) and `corrupt_count` (so `pact doctor` cannot call one corruption).
    #[test]
    fn wait_markers_are_invisible_to_the_lock_scanners() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "hot.rs");
        assert!(acquire(root, "agent-b", "hot.rs", 900, false, None).is_err());

        assert_eq!(peek(root, true).unwrap().len(), 1, "a wait is not a lease");
        assert_eq!(
            corrupt_count(root).unwrap(),
            0,
            "a wait is not a corrupt lock"
        );
    }

    // ---- race-condition guard: verify after rename -----------------------
}
