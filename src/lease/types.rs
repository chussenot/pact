//! The lease record and the pure functions that decide what a lease *is*:
//! how a repo-root-relative path becomes exactly one lock filename, how a TTL
//! is parsed and bounded, and how `(lease, now)` answers "is this expired".
//!
//! Nothing here touches the filesystem except the case-folding probe, which
//! runs once per process in the temp directory. Path spelling and TTL bounding
//! live together because they answer the same question — two spellings of one
//! file, or two dialects of one duration, are the same class of bug, and each
//! has cost this repository a lease.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

/// Below this, a *unitless* `--ttl` is more likely a typo than an intent, and
/// says so. Two minutes: long enough that no plausible "I meant minutes" slip
/// lands above it, short enough that the deliberate short-mutex idiom below is
/// the only thing it ever catches.
const BARE_TTL_SUSPICIOUS_SECS: u64 = 120;

/// `--ttl`: bare seconds, or `<n><unit>` with unit in `smhdw` — the same
/// duration grammar [`crate::audit::parse_since`] takes.
///
/// The unit table is deliberately identical to `parse_since`'s, because two
/// duration dialects in one CLI is how this became a trap: an agent passed
/// `--ttl 20` meaning twenty minutes, got twenty seconds, and its lease lapsed
/// mid-work — so the commit landed under an expired lease, and because the
/// lease *expired* rather than being released, every `pact watch` subscriber on
/// that path got no release diff. A second agent tried `--ttl 3m` and had it
/// rejected. `ttl_grammar_matches_since_grammar` fails if the two ever drift.
///
/// A bare integer still means seconds — scripts pass them and `--ttl 2700` must
/// keep working — so `--ttl 20` will go on meaning twenty seconds forever. Being
/// told is therefore the only thing that saves the next agent, which is why a
/// small bare value warns. It warns rather than rejects: a 20-second lease is a
/// blessed idiom (pact-b7x.3) for a short mutex over a directory some tool is
/// about to write behind you.
pub fn parse_ttl(s: &str) -> Result<u64> {
    let t = s.trim();
    let (num, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len()));
    let n: u64 = num
        .parse()
        .with_context(|| format!("--ttl {t}: expected seconds or a duration like 45m"))?;
    let mult = match unit {
        "" => {
            if n < BARE_TTL_SUSPICIOUS_SECS {
                crate::output::warn(&format!(
                    "warning: --ttl {n} means {n} SECONDS ({}). --ttl takes seconds when \
                     bare, or a unit: {n}m, {n}h, {n}d, {n}w. Holding anyway — if you meant \
                     a short mutex, this is right; if you meant minutes, say {n}m.",
                    human_secs(n as i64)
                ));
            }
            1
        }
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 604800,
        other => {
            return Err(anyhow::anyhow!(
                "--ttl {t}: unknown unit \"{other}\"; use s, m, h, d or w"
            ))
        }
    };
    // Saturate rather than reject: `ttl_as_i64` already clamps to MAX_TTL_SECS,
    // so an absurd input lands on "forever" exactly as `--ttl 99999999999` did
    // before this parser existed.
    Ok(n.saturating_mul(mult))
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
    /// Where the HOLDER was standing when they acquired — `main`, a linked worktree's
    /// name, or `outside`, exactly as `Event::invoked_from` records it.
    ///
    /// On the lock file because the expiry is written by a DIFFERENT process, often
    /// minutes later and often somewhere else, and by then the holder is gone. Without
    /// this the `expired` row inherited the sweeper's location and said something false
    /// about the holder (pact-83r.3 / finding 5).
    ///
    /// Absent — not null — so lock files stay byte-identical to what pact wrote before
    /// this existed, and an old lock still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoked_from: Option<String>,
    /// The git blob id of this path's content when the lease was taken
    /// (pact-8qu), or absent when the file did not exist yet.
    ///
    /// On the lock file as well as on the acquired event because `release` is
    /// the reader: it already loads this struct to check ownership, so the
    /// at-acquire content is one field away rather than a scan back through
    /// the whole event log.
    ///
    /// Deliberately NOT re-stamped by `renew_fs`, which inherits it via
    /// `..existing`: the diff a subscriber eventually receives must be against
    /// the content the holder took responsibility for, and a renew that reset
    /// the baseline would silently hide everything done before it.
    ///
    /// Absent — not null — when there was nothing to hash, so a repo whose
    /// leases are all on not-yet-created files keeps lock files byte-identical
    /// to what pact wrote before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// The holder's harness and declared model (pact-c3y), so a peer refused at
    /// exit 2 and a human reading `lease ls` learn WHAT is holding the path, not
    /// only who.
    ///
    /// On the lock file for the reason `content_hash` and `invoked_from` are: the
    /// readers that need it — the refusal path, `lease ls`, and through it the
    /// TUI's refresh — already load this struct, and the alternative is scanning
    /// the event log for the holder's `acquired` row. That scan would land on the
    /// dashboard's refresh path, which is the one place CLAUDE.md records the
    /// cost of and where an earlier `bd` subprocess had to be removed.
    ///
    /// Absent — not null — when nothing was detected or declared, so a repo whose
    /// agents declare nothing keeps lock files byte-identical to what pact wrote
    /// before this existed, exactly as the three fields above already do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Any field a NEWER lock file has that this compiled struct does not know
    /// about — the mirror image of `branch`/`worktree`'s `default`.
    ///
    /// Forward compat (a future field parses fine on an old binary) was
    /// already free from `#[serde(default)]` on every named field. Backward
    /// compat was not: an OLD binary's `LeaseInfo` has no such field at all,
    /// so any read-modify-write it performs (`renew_fs`, `acquire_inner`'s
    /// `--steal`/expired-reclaim takeover) necessarily reserializes a value
    /// that never carried it — silently dropping it from disk. `flatten`
    /// catches every unrecognized key into this map on read and re-emits it
    /// on write, so a rewrite by a binary that predates a field still
    /// preserves it (pact-m7j.9.8).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct AcquireOutcome {
    pub lease: LeaseInfo,
    pub stolen: bool,
}

/// What `release` actually did — because until pact-mqw.7 three different things
/// printed the same line and exited 0.
///
/// Observed in crucible: agent-08's leases lapsed at 09:12:32Z and their locks
/// were collected. It committed at 09:14:01Z and only then released; pact printed
/// `released lease on tests/corpus.rs` and exited 0. There was nothing to
/// release. The agent was told it had cleanly released a lease it had not held
/// for a minute and a half, and could not learn otherwise from the command it
/// ran — it found out by reading `events.jsonl` afterwards.
///
/// That matters because `release` is where an agent confirms it played by the
/// rules, and the binding rule is commit-before-release. An agent that overruns
/// its TTL, commits, and releases sees an unbroken success path and concludes it
/// complied. It did not: for ninety seconds the path was free, and any peer could
/// have taken it and edited from a different worktree. Nobody did, and that was
/// luck.
///
/// None of these is an error — an idempotent release is a feature. They differ in
/// what they tell the agent about its own conduct, so they differ in what they
/// say.
#[derive(Debug, Clone, Serialize)]
pub enum ReleaseOutcome {
    /// A live lock this agent held, removed. `past_ttl_secs` is set when the
    /// lock was still there but already past its TTL — the holder overran and
    /// got away with it because nobody reclaimed in the window.
    Released { past_ttl_secs: Option<i64> },
    /// `--force` destroyed a different agent's live claim.
    ForceReleased { displaced: String },
    /// No lock, and this agent's own last word on the path in the event log is
    /// an expiry. The lease ended without them, and this call is a no-op.
    AlreadyExpired {
        at: String,
        ttl_secs: Option<u64>,
        /// How long between the lapse and this call: the window in which the
        /// path was free while the agent believed it held it.
        since_secs: Option<i64>,
    },
    /// No lock and no expiry of this agent's on record. A genuinely idempotent
    /// repeat release, or a path never leased here.
    NothingHeld,
}

impl ReleaseOutcome {
    /// The displaced holder, for the one caller that has to go apologise.
    pub fn displaced(&self) -> Option<&str> {
        match self {
            ReleaseOutcome::ForceReleased { displaced } => Some(displaced),
            _ => None,
        }
    }

    /// Did a lock actually go away because of this call? `release --all` counts
    /// only these, and so does the exit-code decision for a multi-path release.
    pub fn removed_a_lock(&self) -> bool {
        matches!(
            self,
            ReleaseOutcome::Released { .. } | ReleaseOutcome::ForceReleased { .. }
        )
    }
}

// `Clone` so a reader can keep a snapshot of what `peek` returned without
// re-scanning `.pact/leases/`: `pact ui` parses every store once per tick and
// hands the views a copy, rather than each view scanning the lock directory for
// itself (pact-pyt.11).
#[derive(Debug, Clone, Serialize)]
pub struct LeaseEntry {
    pub lease: LeaseInfo,
    pub age_secs: i64,
    pub remaining_secs: i64,
    pub expired: bool,
    /// Seconds since this holder's most recent event of ANY kind in
    /// `.pact/events.jsonl`, or `None` when the log has never seen them act.
    ///
    /// Derived, not stored. Every pact command an agent runs appends an event, so
    /// the log is already a liveness signal — it just was not being read as one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_silent_secs: Option<i64>,
    /// Has this holder been quiet for longer than half its own TTL?
    ///
    /// **A stalled holder is strictly worse than a crashed one**, which is why
    /// this exists. A crashed holder's lease expires and peers reclaim it — that
    /// happened three times in the crucible run and worked every time. A stalled
    /// one renews nothing, releases nothing, and blocks peers who are correctly
    /// declining to steal a lease that still reads as live. Seven of ten agents in
    /// that run ended their turn early waiting on a poller that could not wake
    /// them, one of them holding `src/printer.rs`; to `lease ls` that lease was
    /// `active` and its holder alive. It cost more fleet time than every injected
    /// fault combined.
    ///
    /// pact had only the TTL, and the TTL is the slowest possible detector: it
    /// says nothing until the whole lease is over. This says something at half
    /// time, from data pact was already writing.
    ///
    /// Advisory and deliberately weak — it is evidence to message a peer about,
    /// never grounds to `--steal`. A holder can legitimately think for a long time
    /// without running a pact command.
    pub suspect: bool,
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
            // A suspect holder is still `active` to `state()` — the machine
            // answer must not change, because a suspect lease is exactly as
            // unavailable to a peer as any other live one. Only the label an
            // operator reads changes, and it says the age rather than just the
            // word: "quiet 8m12s" is actionable, "SUSPECT" alone invites a steal.
            //
            // Kept under 26 characters — the width `pact ui`'s State column is
            // fixed at so that "stale (reclaimable in 20s)" can never be
            // truncated. A half-read state is how pact-rnc.10 happened, and a
            // half-read "SUSPECT: quiet 8m1…" would be the same mistake with a
            // longer word.
            "active" if self.suspect => match self.holder_silent_secs {
                Some(silent) => format!("SUSPECT: quiet {}", human_secs(silent)),
                None => "SUSPECT: never seen".to_string(),
            },
            other => other.to_string(),
        }
    }
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
/// Why a whitespace path almost always means an unsplit shell variable.
///
/// Its own constant so the sentence lives once and no continuation can smuggle source
/// indentation into it — which the first cut of this message did.
const HINT_WHITESPACE: &str = "If you meant several paths, pass them as separate arguments: \
`pact lease acquire a.rs b.rs` takes two leases, while `pact lease acquire \"a.rs b.rs\"` \
asks for one file whose name contains a space. An unquoted shell variable holding a list \
arrives as ONE argument, because zsh does not word-split it.";

/// Normalize a path a caller is about to CLAIM or WATCH, refusing the shapes that cannot
/// be what they meant.
///
/// Two field findings, one function (pact-83r.4 / findings 3 and 11).
///
/// **Whitespace is refused outright.** From zsh an unquoted variable is not word-split, so
/// `pact lease acquire $FILES` arrives as ONE argument and pact took a single lease on the
/// literal string `a.rs b.rs`. Three agents independently concluded "pact caps multi-path
/// acquires at ~15 paths" — it does not, 40 paths take 0.560s — because past about five
/// the joined string exceeds `NAME_MAX` and the failure surfaces as a raw `os error 36`.
/// No source file in a normal repository has whitespace in its name, so refusing costs
/// nothing and turns the confusing failure into the true one.
///
/// **A path that does not exist is a WARNING, not a refusal**, because watching a
/// not-yet-created file is legitimate and so is leasing one you are about to add. What was
/// missing is any signal at all: six such calls in one run, every one exit 0. The
/// orchestrator lost a lease this way — from the repo root they ran
/// `pact lease acquire src/vm/mod.rs` when the file was at `treadle/src/vm/mod.rs`, pact
/// echoed the path back, and the lease protected nothing while saying it did.
///
/// So the warning prints the RESOLVED path, never the argument as typed. Echoing the input
/// is exactly what made the mistake convincing.
/// The reserved namespace for a lease that stands for something other than a file.
///
/// Agents invented this pattern before pact had a word for it: in the quern run three
/// holds were taken on `.beads` — a directory, not a file — to serialize their own bd
/// writes. It worked, and it was the only non-file path leased in 57 acquires.
/// docs/fleet-patterns.md now blesses it and gives it a home.
pub const MUTEX_PREFIX: &str = ".pact/internal/";

/// Is this lease a mutex rather than a claim on a file?
///
/// **Deliberately does not touch the filesystem.** `audit` reads a log that may
/// describe a repository state that no longer exists, so a `std::fs` check would
/// reclassify a since-deleted file as a mutex and make the same log produce
/// different reports on different days. Two markers, both carried in the log itself:
///
/// - the reserved [`MUTEX_PREFIX`], which is self-describing;
/// - a trailing slash, which is how an agent spells "this is a directory" and how
///   `pact watch` already records a prefix subscription.
///
/// A bare directory name like `.beads` has neither, so quern's own log cannot be
/// reclassified after the fact — new runs using the prefix get clean statistics, and
/// a legacy bare-directory lease keeps appearing as an ordinary path. Said out loud in
/// docs/fleet-patterns.md rather than left as a surprise.
pub fn is_mutex(path: &str) -> bool {
    path.starts_with(MUTEX_PREFIX) || path.ends_with('/')
}

pub(crate) fn resolve_claimable(repo_root: &Path, raw: &str) -> Result<String> {
    if raw.chars().any(char::is_whitespace) {
        anyhow::bail!(
            "path {raw:?} contains whitespace, so it is not a file pact can claim.\n\
             {HINT_WHITESPACE}"
        );
    }
    let relative = normalize_path(repo_root, raw);
    // A reserved key stands for something that is not a file — a merge mutex, a
    // shared store — so it is SUPPOSED to be absent from the working tree, and
    // "this claim protects nothing" is exactly backwards: serializing peers is
    // the entire thing it protects. Warning here trained agents to expect a
    // complaint every time they took the one lock the protocol tells them to
    // take (pact-bsf).
    if !repo_root.join(&relative).exists() && !is_mutex(&relative) {
        crate::output::warn(&format!(
            "note: {relative} does not exist in the working tree (asked for {raw:?}, \
             resolved from the current directory). Fine for a file you are about to \
             create; if you meant an existing one, this claim protects nothing."
        ));
    }
    Ok(relative)
}

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
    // caller's own worktree would have produced.
    let shared_root = crate::repo::RepoContext::resolve(repo_root).shared_root;
    if let Ok(stripped) = folded.strip_prefix(&shared_root) {
        return stripped.to_string_lossy().into_owned();
    }
    // Still no match: the path may have been spelled from a THIRD, non-main
    // linked worktree — neither this process's own root nor the shared/main
    // root (pact-m7j.8.7). `linked_worktree_roots` reads the same kind of
    // plain gitdir-pointer files already used above, so trying every sibling
    // costs no subprocess; it only runs here, once both cheaper candidates
    // have already missed.
    for root in crate::repo::linked_worktree_roots(&shared_root) {
        if let Ok(stripped) = folded.strip_prefix(&root) {
            return stripped.to_string_lossy().into_owned();
        }
    }
    folded.to_string_lossy().into_owned()
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

pub(super) fn is_expired(lease: &LeaseInfo, now: DateTime<Utc>) -> bool {
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

pub(super) fn age_and_remaining(lease: &LeaseInfo, now: DateTime<Utc>) -> (i64, i64) {
    let acquired = parse_acquired(lease);
    let age = (now - acquired).num_seconds();
    (age, ttl_as_i64(lease.ttl_secs) - age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::testutil::*;
    use chrono::Duration;

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

    #[test]
    fn encode_path_replaces_slashes() {
        assert_eq!(encode_path("a/b/c"), "a__b__c");
        assert_eq!(encode_path("single"), "single");
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
                holder_silent_secs: None,
                suspect: false,
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
    fn ttl_takes_units_and_bare_seconds() {
        assert_eq!(parse_ttl("3m").unwrap(), 180);
        assert_eq!(parse_ttl("90m").unwrap(), 5400);
        assert_eq!(parse_ttl("24h").unwrap(), 86400);
        assert_eq!(parse_ttl("7d").unwrap(), 604800);
        assert_eq!(parse_ttl("2w").unwrap(), 1209600);
        assert_eq!(parse_ttl("45s").unwrap(), 45);
        // Bare is seconds, unchanged — including the default and the blessed
        // short-mutex idiom, which must still SUCCEED, not just warn.
        assert_eq!(parse_ttl("2700").unwrap(), DEFAULT_TTL_SECS);
        assert_eq!(parse_ttl("20").unwrap(), 20);
        assert_eq!(parse_ttl(" 3m ").unwrap(), 180);
        // Absurd values saturate onto "forever" rather than erroring, because
        // `ttl_as_i64` clamps and that is what `--ttl 99999999999` already did.
        assert!(ttl_as_i64(parse_ttl("99999999999w").unwrap()) == MAX_TTL_SECS);

        for bad in ["3q", "m", "", "3 m", "-5", "3.5h", "1h30m"] {
            assert!(parse_ttl(bad).is_err(), "{bad} should not parse");
        }
    }

    /// The whole point of finding 7: `--ttl` and `--since` must not be two
    /// dialects. This fails the day either unit table is edited alone.
    #[test]
    fn ttl_grammar_matches_since_grammar() {
        for d in ["45s", "3m", "90m", "24h", "7d", "2w"] {
            let back = (Utc::now() - crate::audit::parse_since(d).unwrap()).num_seconds();
            let ttl = parse_ttl(d).unwrap() as i64;
            assert!(
                (ttl - back).abs() <= 1,
                "--ttl {d} = {ttl}s but --since {d} = {back}s back"
            );
        }
        // ...and both reject the same unit.
        assert!(parse_ttl("3y").is_err());
        assert!(crate::audit::parse_since("3y").is_err());
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
            invoked_from: None,
            content_hash: None,
            harness: None,
            model: None,
            extra: BTreeMap::new(),
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
                invoked_from: None,
                content_hash: None,
                harness: None,
                model: None,
                extra: BTreeMap::new(),
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
            invoked_from: None,
            content_hash: None,
            harness: None,
            model: None,
            extra: BTreeMap::new(),
        };
        // Years past ttl+grace, not just minutes: a forward jump, not a slow
        // overrun.
        let jumped_now = acquired_at + Duration::days(800);
        assert!(
            !is_expired(&lease, jumped_now),
            "a lease acquired 60s ago must not auto-expire just because `now` jumped ~2 years forward"
        );
    }
}
