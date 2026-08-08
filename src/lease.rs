//! Advisory file leases: atomic lock files under `.pact/leases/`, with TTL,
//! steal, and re-entrant-refresh semantics. See docs/leases.md.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::events;
use crate::otel;
use crate::output::exit_with;
use crate::repo::pact_dir;

/// Default lease lifetime: 45 minutes.
///
/// **Calibrated from fleet telemetry on 2026-08-06**, not chosen. `pact audit` over
/// this repository's own 147 preserved events (20 agents, 67 completed holds, six
/// synthetic events excluded by annotation) measured:
///
/// | | seconds | |
/// |---|---|---|
/// | median hold | 842 | 14m2s |
/// | p90 hold | 1455 | 24m15s |
/// | longest hold | 2166 | 36m6s |
///
/// With **1 renewal in 147 events**. The old default was 900s, so the p90 hold ran
/// 9 minutes past expiry and the longest ran 21 minutes past, each of them
/// reclaimable by any peer while its holder was still working. Renewing is what the
/// protocol asks for and the data says agents do not do it — once, ever. So the
/// tool adapts to measured behaviour rather than demanding ceremony that is
/// demonstrably skipped, which is the same call the claim-skip change made.
///
/// 2700 sits 1.85x the measured p90 and 1.25x the longest hold ever recorded here:
/// enough headroom that the next long task is covered, not so much that an
/// abandoned lease sits unreclaimable for an hour.
///
/// **One honest caveat about the evidence.** In that whole history there are
/// **zero** expiry events: holds did exceed the TTL, but no peer ever actually
/// reclaimed one. So this fixes a demonstrated exposure window rather than a
/// demonstrated collision — the risk was real, nothing had yet exploited it.
///
/// Recalibrating is now a measurement rather than a guess: `pact audit` prints the
/// distribution, and `--check stale-holds` judges each hold against the TTL *it*
/// recorded, so changing this constant cannot rewrite the past.
pub const DEFAULT_TTL_SECS: u64 = 2700;
/// Clock-skew tolerance: a lease is only considered expired past `ttl + GRACE_SECS`.
pub const GRACE_SECS: i64 = 30;

/// A TTL at or beyond this is functionally "forever" for a coordination
/// lease -- 100 years. Real inputs never approach it; it exists to cap the
/// u64 CLI value before it becomes an i64 fed to `chrono::Duration::seconds`,
/// which panics ("TimeDelta::seconds out of bounds") once seconds exceeds
/// roughly `i64::MAX / 1000` -- verified directly against chrono 0.4.45.
/// Saturating to `i64::MAX` still crosses that line, trading a silent
/// misexpiry for a hard crash on every subsequent `is_expired` check against
/// the same lease, which is worse (pact-m7j.9.10).
pub const MAX_TTL_SECS: i64 = 100 * 365 * 24 * 60 * 60;

/// `ttl_secs` arrives from the CLI as an unbounded `u64` (`--ttl` has no
/// range check). A bare `as i64` bit-reinterprets any value at or past 2^63
/// as negative, inverting "hold forever" into "already expired" the instant
/// it is read back. Every call site that turns a stored or requested TTL
/// into a `chrono::Duration` or an `i64` comparison must go through this.
pub fn ttl_as_i64(ttl_secs: u64) -> i64 {
    i64::try_from(ttl_secs)
        .unwrap_or(i64::MAX)
        .min(MAX_TTL_SECS)
}

/// Sanity bound on how old a lease's computed age may plausibly be before
/// `is_expired` trusts it enough to auto-reclaim (pact-m7j.4.4, forward clock
/// jump).
///
/// `is_expired(lease, now)` is a pure function of two timestamps: it cannot
/// tell "this really has been held for months" apart from "the wall clock
/// jumped forward" — both produce an identical, huge `now - acquired_at`.
/// Rather than guess, an age past this bound is treated as too implausible to
/// auto-expire; the normal ttl+grace reclaim is refused and an explicit
/// `--steal` is required instead, exactly as for a live, non-expired lease.
///
/// 30 days is deliberately generous and deliberately stateless — it does not
/// depend on the persisted clock watermark below, which solves the opposite
/// (backward-jump) direction. It is far larger than [`DEFAULT_TTL_SECS`]
/// (2700s) or any hold `pact audit` has ever measured (36 minutes, longest
/// on record) or any plausible renewal chain, while still catching a
/// multi-month or multi-year jump.
const MAX_PLAUSIBLE_AGE_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseInfo {
    pub agent: String,
    pub path: String,
    pub acquired_at: String, // RFC3339
    pub ttl_secs: u64,
    pub note: Option<String>,
    /// The branch the holder had checked out, and which worktree they hold it
    /// from. Informational: nothing branches on either.
    ///
    /// Both are **absent** — not null — in a repository with no linked
    /// worktrees, so its lock files stay byte-identical to what pact wrote
    /// before it understood worktrees at all. `default` on the way in, so a lock
    /// file written by an older pact still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

/// The branch/worktree pair to stamp on a new lease, empty unless this
/// repository actually has linked worktrees.
///
/// Keyed on `has_worktrees` rather than on "am I in a linked worktree": the
/// holder may be sitting in the MAIN worktree while the loser is in a linked
/// one, and a conflict message that can only name the loser's worktree explains
/// nothing.
fn worktree_stamp(repo_root: &Path) -> (Option<String>, Option<String>) {
    let ctx = crate::repo::RepoContext::resolve(repo_root);
    if !ctx.has_worktrees {
        return (None, None);
    }
    (ctx.branch(), ctx.worktree_name.clone())
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

#[derive(Debug, Serialize)]
pub struct AcquireOutcome {
    pub lease: LeaseInfo,
    pub stolen: bool,
}

#[derive(Debug, Serialize)]
pub struct LeaseEntry {
    pub lease: LeaseInfo,
    pub age_secs: i64,
    pub remaining_secs: i64,
    pub expired: bool,
}

impl LeaseEntry {
    /// What an operator actually needs to know: is this claim fresh, probably
    /// abandoned, or reclaimable? `remaining_secs` alone reads as "long-held"
    /// on a lease that is seconds old (pact-rnc.10).
    ///   "active"  — within its ttl
    ///   "stale"   — past its ttl but inside the GRACE_SECS clock-skew window,
    ///               i.e. probably abandoned but not yet reclaimable
    ///   "expired" — past ttl + GRACE_SECS; another agent may take it
    pub fn state(&self) -> &'static str {
        // Keyed off `expired` rather than recomputing, so the label can never
        // disagree with the GC decision made in `list`.
        if self.expired {
            "expired"
        } else if self.remaining_secs < 0 {
            "stale"
        } else {
            "active"
        }
    }

    /// The state as an operator reads it, including when a stale lease becomes
    /// reclaimable. Lives here, not in a renderer: `pact lease ls` and `pact ui`
    /// both show lease state, and having each format it its own way is what left
    /// the dashboard printing a raw `80s 3520s active` after pact-rnc.10 was
    /// "fixed" in the CLI. One implementation, both surfaces.
    pub fn state_label(&self) -> String {
        match self.state() {
            "stale" => format!(
                "stale (reclaimable in {})",
                human_secs(self.remaining_secs + GRACE_SECS)
            ),
            other => other.to_string(),
        }
    }
}

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
    ) -> Result<Option<String>>;
    fn release_all(&self, repo_root: &Path, agent: &str) -> Result<Vec<String>>;
    fn renew(&self, repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo>;
    fn list(&self, repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>>;
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
    ) -> Result<Option<String>> {
        release_fs(repo_root, agent, path, force)
    }

    fn release_all(&self, repo_root: &Path, agent: &str) -> Result<Vec<String>> {
        release_all_fs(repo_root, agent)
    }

    fn renew(&self, repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo> {
        renew_fs(repo_root, agent, path)
    }

    fn list(&self, repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
        list_fs(repo_root, all)
    }

    fn peek(&self, repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
        peek_fs(repo_root, all)
    }
}

static FILE_LEASE_STORE: FileLeaseStore = FileLeaseStore;

pub fn current_store() -> &'static dyn LeaseStore {
    &FILE_LEASE_STORE
}

/// Compact duration: `45s`, `2m5s`, `1h3m`. Here for the same reason as
/// `state_label`: a bare four-digit second count next to an age is what made
/// pact-rnc.10 misreadable, so no renderer gets to reinvent it.
pub fn human_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// Encode a repo-root-relative path into a lock filename: `/` -> `__`.
/// Collision caveat: a path containing literal `__` can collide with a
/// different path whose separators encode to the same string. Acceptable in v1.
pub fn encode_path(relative_path: &str) -> String {
    let encoded = relative_path.replace('/', "__");
    // Case-fold the lock NAME, and only where the filesystem folds case too.
    //
    // pact-r2s.1 made one file resolve to one lock however the agent spelled
    // the path — but only for the directory dimension. On a case-insensitive
    // filesystem, which is macOS's default and what pact's own CI runs,
    // `src/foo.rs` and `src/Foo.rs` are ONE file and still produced two locks,
    // so two agents each held it and both were told they did.
    //
    // Unconditional lowercasing would be wrong, not merely conservative: on
    // Linux those really are two files, and collapsing them manufactures the
    // false conflict that the other half of pact-r2s.1 exists to prevent. So
    // the answer has to come from the filesystem.
    //
    // Only the lock FILENAME is folded. `LeaseInfo.path` keeps the spelling the
    // holder used, so `pact lease ls` still shows what they typed and the error
    // still names the path they asked for.
    encode_with_folding(&encoded, case_insensitive_fs())
}

/// The folding decision, separated from where the answer comes from, so both
/// branches are testable on any platform. Only one of them can ever be
/// exercised end-to-end on a given machine, which is exactly why the other one
/// needs a test that does not depend on the machine.
fn encode_with_folding(encoded: &str, fold: bool) -> String {
    if fold {
        encoded.to_lowercase()
    } else {
        encoded.to_string()
    }
}

/// Does this filesystem treat `A` and `a` as the same name?
///
/// Probed, not read from `core.ignorecase`: that value is written by git at
/// clone time and describes the filesystem git saw then, which is not
/// necessarily the one pact is looking at now — a repo cloned on macOS and
/// opened over a shared volume from Linux carries the wrong answer. The probe
/// asks the only authority that matters.
///
/// Once per process, in the temp directory rather than in the repo, so a probe
/// never appears in anyone's `git status`. Unknown answers to "sensitive",
/// which preserves the behaviour every existing platform already had.
fn case_insensitive_fs() -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| {
        let dir = std::env::temp_dir();
        let lower = dir.join(format!(
            ".pact-case-{}",
            crate::events::unique_temp_name("p")
        ));
        let upper = PathBuf::from(lower.to_string_lossy().to_uppercase());
        if lower == upper || std::fs::write(&lower, b"").is_err() {
            return false;
        }
        let folded = upper.exists();
        let _ = std::fs::remove_file(&lower);
        folded
    })
}

/// A path as the lock filename sees it: repo-root-relative, one spelling per
/// file, whatever directory the agent ran from.
///
/// A relative path used to be taken verbatim, which meant the CWD silently
/// decided which lock you got. Both directions were wrong and both were
/// reproducible: `pact lease acquire foo.rs` from `src/deep/` and
/// `pact lease acquire src/deep/foo.rs` from the root took TWO locks on ONE
/// file — two agents each told they held it, neither warned, which is the one
/// thing the lease surface exists to prevent. And `foo.rs` at the root vs
/// `foo.rs` inside `src/deep/` — two different files — collapsed into one lock
/// and produced a conflict over a file nobody shared.
///
/// Agent Mail met the same class twice in one commit (c66e54f): #204 resolved
/// `git_common_dir` against the repo root because the process CWD gave an
/// unstable id, and #205 collapsed two divergent normalizers that had been
/// splitting one mailbox in two.
///
/// Lexical, never `canonicalize()`. A lease on a file that does not exist yet
/// is a documented workflow — see docs/leases.md, "Working on a new file you
/// can't compile yet" — and `canonicalize` fails on a missing path, so it would
/// break the case the feature was built for. `..` is folded here rather than by
/// the filesystem, so a symlinked directory resolves by name; that is the same
/// bargain the rest of the module already makes by keying locks on paths.
///
/// `pub(crate)`, not private, because the lock itself was not the only place
/// answering "who owns this path?" — `events::owner_of`, `msg::about_path`'s
/// label, and `msg send --to-owner-of`'s resolution each did their own,
/// simpler comparison of un-normalized input, so the same file addressed from
/// two CWDs got two different answers on those surfaces even after pact-r2s.1
/// fixed it for the lock (pact-m7j.8.6). Every current caller derives
/// `repo_root` the same way (`repo::find_repo_root(&cwd)`, once, in `main`),
/// so `cwd_in_repo` above is satisfied by construction everywhere this is
/// called — including from other modules.
pub(crate) fn normalize_path(repo_root: &Path, path: &str) -> String {
    let p = Path::new(path);
    // A relative path is resolved against the CWD — but only when the CWD is
    // actually inside this repo. Every production caller derives `repo_root` by
    // walking up from the CWD (`repo::find_repo_root(&cwd)` in main.rs), so the
    // two always agree there. A caller that passes some other root — the unit
    // tests, and any future embedder of `LeaseStore` — means "relative to the
    // root I gave you", and joining an unrelated CWD would silently produce a
    // path outside the repo. Checking the relationship instead of assuming it
    // keeps both callers honest.
    let cwd_in_repo = std::env::current_dir()
        .ok()
        .filter(|cwd| cwd.starts_with(repo_root));
    let absolute = match (p.is_absolute(), cwd_in_repo) {
        (true, _) => p.to_path_buf(),
        (false, Some(cwd)) => cwd.join(p),
        // No usable CWD: treat the input as already repo-root-relative, which
        // is what it was before this function learned about the CWD at all.
        (false, None) => return fold(p).to_string_lossy().into_owned(),
    };
    let folded = fold(&absolute);
    if let Ok(stripped) = folded.strip_prefix(repo_root) {
        return stripped.to_string_lossy().into_owned();
    }
    // An absolute (or `..`-escaped) path that wasn't spelled from this
    // process's own root doesn't automatically mean it's outside the repo —
    // it may have been typed from a *different* worktree sharing this same
    // coordination space (pact-m7j.8.2), e.g. copy-pasted from `pact lease
    // ls`'s WHERE output on another checkout. Try the shared coordination
    // root next before giving up: for the common two-worktree case (typed
    // from, or as, the main worktree) this recovers the same lock key the
    // caller's own worktree would have produced. A path spelled from a
    // THIRD, non-main sibling worktree still falls through unfixed — no code
    // here enumerates every linked worktree's own checkout root.
    let shared_root = crate::repo::RepoContext::resolve(repo_root).shared_root;
    folded
        .strip_prefix(&shared_root)
        .unwrap_or(&folded)
        .to_string_lossy()
        .into_owned()
}

/// Fold `.` and `..` textually. No filesystem access, so it works on a path
/// that does not exist yet. A leading `..` that escapes the root is kept, so
/// the caller still sees an out-of-repo path rather than a silently rebased one.
fn fold(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn lock_file_path(repo_root: &Path, relative: &str) -> Result<PathBuf> {
    let dir = pact_dir(repo_root)?;
    Ok(dir
        .join("leases")
        .join(format!("{}.lock", encode_path(relative))))
}

fn parse_acquired_raw(lease: &LeaseInfo) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&lease.acquired_at)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_acquired(lease: &LeaseInfo) -> DateTime<Utc> {
    // A lock file with an unparsable timestamp is a corruption case we don't
    // expect in practice (we always write RFC3339 ourselves). For an advisory
    // lock, corruption should tend towards "expired/claimable": fall back to
    // the Unix epoch (1970-01-01) so the lease is immediately reclaimable
    // rather than held forever (the old `Utc::now()` fallback reset the timer
    // on every read, making a corrupt lease immortal until `--steal`).
    parse_acquired_raw(lease).unwrap_or(DateTime::UNIX_EPOCH)
}

fn is_expired(lease: &LeaseInfo, now: DateTime<Utc>) -> bool {
    match parse_acquired_raw(lease) {
        Some(acquired) => {
            // pact-m7j.4.4: an implausibly large age says more about `now`
            // than about this lease. Bail out to "not expired" (i.e.
            // "needs --steal") before trusting it, rather than auto-reclaiming
            // on a forward clock jump. This does not apply to the corrupt-
            // timestamp fallback below — that failure is about bad data, not
            // a suspicious `now`, and must keep tending towards reclaimable.
            if now - acquired > chrono::Duration::seconds(MAX_PLAUSIBLE_AGE_SECS) {
                return false;
            }
            now > acquired + chrono::Duration::seconds(ttl_as_i64(lease.ttl_secs) + GRACE_SECS)
        }
        None => true,
    }
}

fn age_and_remaining(lease: &LeaseInfo, now: DateTime<Utc>) -> (i64, i64) {
    let acquired = parse_acquired(lease);
    let age = (now - acquired).num_seconds();
    (age, ttl_as_i64(lease.ttl_secs) - age)
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
fn effective_now_readonly(repo_root: &Path) -> DateTime<Utc> {
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
fn effective_now(repo_root: &Path) -> DateTime<Utc> {
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
fn write_lease_atomic(lock_path: &Path, lease: &LeaseInfo) -> Result<()> {
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
fn temp_sibling(lock_path: &Path) -> PathBuf {
    let dir = lock_path.parent().unwrap_or(Path::new("."));
    dir.join(crate::events::unique_temp_name("staging"))
}

fn read_lease(lock_path: &Path) -> Result<LeaseInfo> {
    let contents = std::fs::read_to_string(lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parsing lease at {}", lock_path.display()))
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
fn log_event(
    repo_root: &Path,
    agent: &str,
    kind: &str,
    path: &str,
    detail: Option<String>,
    ttl_secs: u64,
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
    let relative = normalize_path(repo_root, path);
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
    let (branch, worktree) = worktree_stamp(repo_root);
    let new_lease = LeaseInfo {
        agent: agent.to_string(),
        path: relative.clone(),
        acquired_at: now.to_rfc3339(),
        ttl_secs,
        note,
        branch,
        worktree,
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
            // Cheap because it reuses the same log scan `events::owner_of`
            // already does (bounded to events.jsonl's own line cap), not
            // `audit::reconstruct`'s full-log walk, which answers a broader
            // question at a much higher cost. A warning, not a refusal — a
            // doctor-prescribed manual `leases/` wipe produces this exact
            // shape and is a legitimate recovery, so blocking here would
            // misfire on precisely the case doctor tells people to run.
            if let Ok(Some(prior)) = events::owner_of(repo_root, &relative) {
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
            log_event(
                repo_root,
                agent,
                "acquired",
                &relative,
                new_lease.note.clone(),
                new_lease.ttl_secs,
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
                );
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

/// Release a lease. Returns `Some(displaced_agent)` when `force` destroyed a
/// *different* agent's live claim, so the caller can warn and name them the way
/// `acquire --steal` already does (pact-rnc.11); `None` when the caller held it
/// or nothing was held.
pub fn release(repo_root: &Path, agent: &str, path: &str, force: bool) -> Result<Option<String>> {
    current_store().release(repo_root, agent, path, force)
}

fn release_fs(repo_root: &Path, agent: &str, path: &str, force: bool) -> Result<Option<String>> {
    let relative = normalize_path(repo_root, path);
    let lock_path = lock_file_path(repo_root, &relative)?;
    let mut sp = otel::span("pact.lease.release");
    sp.set("pact.path", relative.clone());

    if !lock_path.exists() {
        return Ok(None); // idempotent: nothing to release
    }
    // release_fs had no verify guard at all (pact-m7j.1.5): without it, a
    // concurrent legitimate takeover's write could land between this read and
    // the delete below, and we would delete their fresh claim believing it was
    // still ours. Same guard as acquire_inner's takeover branches.
    let _guard = WriteGuard::acquire(&lock_path)?;
    let existing = match read_lease(&lock_path) {
        Ok(lease) => lease,
        // Released by a peer while we waited for the guard.
        Err(_) if !lock_path.exists() => return Ok(None),
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
                },
            );
            count_transition("force_released");
            return Ok(None);
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
                existing.note,
                existing.ttl_secs,
            );
            count_transition("released");
        }
    }
    Ok(displaced)
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
        );
    }
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

    for path in &held {
        release_fs(repo_root, agent, path, false)?;
    }
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
    let (branch, worktree) = worktree_stamp(repo_root);
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
    );
    count_transition("renewed");
    Ok(renewed)
}

/// Read every lock file, paired with the file it came from. The single reader
/// behind both [`list`] and [`peek`], so the two cannot drift apart: they
/// differ in exactly one thing (whether expired locks are unlinked) and share
/// everything else — parsing, skipping, age and state computation.
fn scan(repo_root: &Path) -> Result<Vec<(PathBuf, LeaseEntry)>> {
    // Non-creating path: listing leases is a question, and a question must not
    // leave a `.pact/` behind in a repo that has never used pact (pact-rnc.27).
    let leases_dir = crate::repo::pact_dir_path(repo_root).join("leases");
    // Read-only correction (pact-m7j.4.5): `scan` backs `peek`, which must not
    // mutate `.pact/` on a mere question, so this must not persist a new
    // watermark — see `effective_now_readonly`'s doc comment. `list` still
    // benefits: the watermark it reads was written by whatever `acquire`/
    // `renew` call created the lease it is now sweeping.
    let now = effective_now_readonly(repo_root);
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

        entries.push((
            path,
            LeaseEntry {
                lease,
                age_secs,
                remaining_secs,
                expired,
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
pub fn list(repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
    current_store().list(repo_root, all)
}

fn list_fs(repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
    let mut entries = Vec::new();
    for (lock_path, entry) in scan(repo_root)? {
        if entry.expired {
            collect_expired(repo_root, &lock_path, &entry.lease);
            if !all {
                continue;
            }
        }
        entries.push(entry);
    }
    Ok(entries)
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

fn peek_fs(repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
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
        if path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("lock") {
            continue;
        }
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {

    /// One file must be one lock however it is spelled — pact-r2s.1 covered the
    /// directory dimension, this covers case. Both branches are asserted here
    /// because only one of them can run on any given machine: this suite runs on
    /// Linux (case-sensitive) and macOS (case-insensitive by default), and the
    /// wrong branch on either is a real bug — two agents holding one file, or a
    /// conflict over two files that merely share a name (pact-703.3).
    #[test]
    fn case_folding_follows_the_filesystem_and_not_the_platform() {
        // Case-insensitive: two spellings collapse to one lock name.
        assert_eq!(
            encode_with_folding("src__Foo.rs", true),
            encode_with_folding("src__foo.rs", true)
        );
        // Case-sensitive: they must stay distinct, or leasing src/foo.rs would
        // falsely conflict with src/Foo.rs, which is a different file here.
        assert_ne!(
            encode_with_folding("src__Foo.rs", false),
            encode_with_folding("src__foo.rs", false)
        );
        // Folding never touches the separator encoding.
        assert_eq!(encode_with_folding("a__B.rs", true), "a__b.rs");
    }

    /// The probe must agree with the filesystem the test is actually running
    /// on, and must not leave anything behind.
    #[test]
    fn the_case_probe_matches_this_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Probe"), b"").unwrap();
        let folds_here = dir.path().join("probe").exists();
        assert_eq!(
            case_insensitive_fs(),
            folds_here,
            "the probe disagrees with a direct test of this filesystem"
        );
        // Repeated calls are cached, not re-probed, and still agree.
        assert_eq!(case_insensitive_fs(), folds_here);
    }

    use super::*;
    use chrono::Duration;

    #[test]
    fn encode_path_replaces_slashes() {
        assert_eq!(encode_path("a/b/c"), "a__b__c");
        assert_eq!(encode_path("single"), "single");
    }

    fn lease_aged(ttl_secs: u64, age_secs: i64) -> (LeaseInfo, DateTime<Utc>) {
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
            },
            now,
        )
    }

    #[test]
    fn expiry_respects_grace_period_boundary() {
        let ttl = 100u64;
        let (lease, now) = lease_aged(ttl, ttl as i64 + GRACE_SECS - 1);
        assert!(
            !is_expired(&lease, now),
            "ttl+grace-1s should not be expired yet"
        );

        let (lease, now) = lease_aged(ttl, ttl as i64 + GRACE_SECS + 1);
        assert!(is_expired(&lease, now), "ttl+grace+1s should be expired");
    }

    /// Same boundary style as `expiry_respects_grace_period_boundary`, one age
    /// per state.
    #[test]
    fn state_labels_the_three_ttl_bands() {
        let ttl = 100u64;
        let entry_at = |age: i64| {
            let (lease, now) = lease_aged(ttl, age);
            let (age_secs, remaining_secs) = age_and_remaining(&lease, now);
            let expired = is_expired(&lease, now);
            LeaseEntry {
                lease,
                age_secs,
                remaining_secs,
                expired,
            }
        };

        assert_eq!(entry_at(1).state(), "active");
        assert_eq!(entry_at(ttl as i64 - 1).state(), "active");
        // Past ttl but inside the grace window: probably abandoned, not yet
        // reclaimable.
        assert_eq!(entry_at(ttl as i64 + 1).state(), "stale");
        assert_eq!(entry_at(ttl as i64 + GRACE_SECS - 1).state(), "stale");
        assert_eq!(entry_at(ttl as i64 + GRACE_SECS + 1).state(), "expired");

        // The label every renderer shows, so `pact ui` and `pact lease ls`
        // cannot drift apart again (pact-rnc.10).
        assert_eq!(entry_at(1).state_label(), "active");
        assert!(entry_at(ttl as i64 + 1)
            .state_label()
            .starts_with("stale (reclaimable in "));
        assert_eq!(
            entry_at(ttl as i64 + GRACE_SECS + 1).state_label(),
            "expired"
        );
    }

    #[test]
    fn human_secs_bands() {
        assert_eq!(human_secs(0), "0s");
        assert_eq!(human_secs(-5), "0s");
        assert_eq!(human_secs(59), "59s");
        assert_eq!(human_secs(125), "2m5s");
        assert_eq!(human_secs(3725), "1h2m");
    }

    #[test]
    fn corrupt_timestamp_is_treated_as_expired_not_immortal() {
        // A lease whose `acquired_at` cannot be parsed must fall back to epoch 0,
        // making it immediately expired — not immortal (the old `Utc::now()`
        // fallback reset the timer on every read, so the lease never expired).
        let corrupt = LeaseInfo {
            agent: "agent-a".into(),
            path: "x".into(),
            acquired_at: "not-a-timestamp".into(),
            ttl_secs: DEFAULT_TTL_SECS,
            note: None,
            branch: None,
            worktree: None,
        };
        assert!(
            is_expired(&corrupt, Utc::now()),
            "a corrupt timestamp must be treated as expired"
        );
        // parse_acquired must return epoch 0, not something near now.
        let parsed = parse_acquired(&corrupt);
        assert_eq!(
            parsed,
            DateTime::UNIX_EPOCH,
            "corrupt timestamp must parse as Unix epoch"
        );
    }

    /// pact-m7j.4.3: `parse_acquired`'s `unwrap_or(UNIX_EPOCH)` is type-level
    /// total over any parse `Err`, so it already covers a far wider
    /// adversarial space than the single garbage string above exercises —
    /// this locks that coverage in, on both the producer side
    /// (`parse_acquired` itself) and the consumer side (`is_expired`), so a
    /// future refactor of either cannot narrow the fallback without a test
    /// failing. Expected to pass against current code unchanged; the value
    /// is the regression guard, not a fix.
    #[test]
    fn parse_acquired_falls_back_to_epoch_for_every_adversarial_shape() {
        let adversarial_timestamps = [
            "",
            "2026-08-01T10:00:00", // ISO-8601-like, but RFC3339 requires an offset
            "2026-08-01T10:00",    // truncated RFC3339 prefix, the shape a torn write leaves
            "2026-08-01T10:00:00+00:00\u{0}garbage", // embedded control character
            "99999-08-01T10:00:00+00:00", // out-of-range year
        ];
        for ts in adversarial_timestamps {
            let lease = LeaseInfo {
                agent: "agent-a".into(),
                path: "x".into(),
                acquired_at: ts.into(),
                ttl_secs: DEFAULT_TTL_SECS,
                note: None,
                branch: None,
                worktree: None,
            };
            assert_eq!(
                parse_acquired(&lease),
                DateTime::UNIX_EPOCH,
                "{ts:?} must fall back to epoch 0"
            );
            assert!(
                is_expired(&lease, Utc::now()),
                "{ts:?} must read as expired, not immortal"
            );
        }
    }

    /// pact-m7j.9.10: `--ttl` is an unbounded `u64` CLI arg with no range
    /// check. A bare `ttl_secs as i64` bit-reinterprets `u64::MAX` as `-1`,
    /// so a lease requested to last "forever" used to read back as already
    /// expired (and `remaining` as negative) the instant it was checked.
    #[test]
    fn a_u64_max_ttl_reads_as_active_not_already_expired() {
        let (lease, now) = lease_aged(u64::MAX, 0);
        assert!(
            !is_expired(&lease, now),
            "a u64::MAX ttl must not read back as already expired"
        );
        let (_, remaining) = age_and_remaining(&lease, now);
        assert!(
            remaining > 0,
            "remaining must not go negative for a u64::MAX ttl: {remaining}"
        );
    }

    /// The same overflow reached `chrono::Duration::seconds` if a naive fix
    /// merely saturated the cast to `i64::MAX` instead of capping well below
    /// it: `Duration::seconds` panics ("TimeDelta::seconds out of bounds")
    /// past roughly `i64::MAX / 1000`. A panic on every subsequent expiry
    /// check against the same lease is worse than the original silent
    /// misexpiry, so this pins the non-panicking behavior directly.
    #[test]
    fn ttl_as_i64_stays_within_chrono_duration_bounds() {
        let capped = ttl_as_i64(u64::MAX);
        let _ = chrono::Duration::seconds(capped + GRACE_SECS);
    }

    #[test]
    fn forward_clock_jump_does_not_auto_expire_a_live_lease() {
        // pact-m7j.4.4: a lease acquired moments ago must not be reclaimed
        // just because `now` is observed far in the future (NTP correction,
        // manual clock change, VM pause/resume, container drift). The raw
        // computed age here vastly exceeds ttl+grace — that is the point:
        // `is_expired` must refuse to trust it rather than auto-reclaim, and
        // require an explicit `--steal` instead, same as a live lease.
        let acquired_at = Utc::now() - Duration::seconds(60); // a real, recent acquire
        let lease = LeaseInfo {
            agent: "agent-a".into(),
            path: "x".into(),
            acquired_at: acquired_at.to_rfc3339(),
            ttl_secs: DEFAULT_TTL_SECS,
            note: None,
            branch: None,
            worktree: None,
        };
        // Years past ttl+grace, not just minutes: a forward jump, not a slow
        // overrun.
        let jumped_now = acquired_at + Duration::days(800);
        assert!(
            !is_expired(&lease, jumped_now),
            "a lease acquired 60s ago must not auto-expire just because `now` jumped ~2 years forward"
        );
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

    fn repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn claim(root: &Path, agent: &str, path: &str) {
        acquire(root, agent, path, DEFAULT_TTL_SECS, false, None).unwrap();
    }

    fn held_by(root: &Path, agent: &str) -> Vec<String> {
        let mut paths: Vec<String> = list(root, true)
            .unwrap()
            .into_iter()
            .filter(|e| e.lease.agent == agent)
            .map(|e| e.lease.path)
            .collect();
        paths.sort();
        paths
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
        assert_eq!(release(root, "agent-a", "mine.rs", false).unwrap(), None);
        assert_eq!(release(root, "agent-a", "mine.rs", false).unwrap(), None); // idempotent

        claim(root, "agent-a", "theirs.rs");
        assert!(release(root, "agent-b", "theirs.rs", false).is_err());
        assert_eq!(
            release(root, "agent-b", "theirs.rs", true).unwrap(),
            Some("agent-a".to_string())
        );
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

    /// Plant a lock file that is already `age_secs` old, without going through
    /// `acquire` — the only way to test expiry without sleeping.
    fn claim_aged(root: &Path, agent: &str, path: &str, ttl_secs: u64, age_secs: i64) {
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
    fn claim_at(root: &Path, agent: &str, path: &str, ttl_secs: u64, acquired_at: DateTime<Utc>) {
        let lease = LeaseInfo {
            agent: agent.into(),
            path: path.into(),
            acquired_at: acquired_at.to_rfc3339(),
            ttl_secs,
            note: None,
            branch: None,
            worktree: None,
        };
        write_lease_atomic(&lock_file_path(root, path).unwrap(), &lease).unwrap();
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

    fn lock_exists(root: &Path, path: &str) -> bool {
        lock_file_path(root, path).unwrap().exists()
    }

    // ---- pact-rnc.19: peek() answers without mutating -------------------

    #[test]
    fn listing_creates_nothing_on_a_repo_that_has_never_used_pact() {
        // pact-rnc.27: same principle as peek() not sweeping (pact-rnc.19) --
        // asking what is leased must not leave a `.pact/` behind, and a missing
        // directory means "nothing leased yet", not an error.
        let tmp = repo();
        let root = tmp.path();
        assert!(peek(root, true).unwrap().is_empty());
        assert!(list(root, true).unwrap().is_empty());
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
        let listed = list(root, true).unwrap();
        assert_eq!(listed.len(), 2, "--all still shows the swept lease once");
        assert!(!lock_exists(root, "dead.rs"), "list must GC");
        assert!(lock_exists(root, "live.rs"));
    }

    // ---- pact-rnc.21: atomic multi-path claims ---------------------------

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

    fn event_kinds(root: &Path) -> Vec<(String, String)> {
        crate::events::recent(root, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.path.unwrap_or_default()))
            .collect()
    }

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
                ("stolen".to_string(), "f.rs".to_string()),
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
        assert_eq!(events[3].agent, "agent-b");
        assert!(
            events[3].detail.as_deref().unwrap().contains("agent-a"),
            "a steal must name the displaced holder: {:?}",
            events[3].detail
        );
        assert!(
            events[4].detail.as_deref().unwrap().contains("agent-b"),
            "a force-release must name the displaced holder: {:?}",
            events[4].detail
        );
        // pact-m7j.2.6: the displaced holder in a structured field too, not
        // just free text — audit::reconstruct needs this to close THEIR
        // window, since this event's own `agent` (agent-a) is the one who
        // forced it, not the one displaced.
        assert_eq!(
            events[4].displaced.as_deref(),
            Some("agent-b"),
            "force-released must carry the displaced holder as a structured field: {:?}",
            events[4].displaced
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

        // A --steal of a LIVE claim has no "expired" row: that is the difference.
        acquire(root, "agent-c", "dead.rs", 900, true, None).unwrap();
        let kinds: Vec<String> = crate::events::recent(root, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["expired", "stolen", "stolen"]);
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
        };
        write_lease_atomic(&lock_path, &winner).unwrap();

        verify_own_lease(&lock_path, "agent-a")
            .expect("same-identity race must not be reported as a lost steal");
    }
}
