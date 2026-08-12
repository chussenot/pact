//! Lease event log: an append-only JSONL feed at `.pact/events.jsonl`.
//!
//! Why this file exists at all (pact-rnc.13): `lease ls` shows only the
//! instantaneous set, and releasing a lease *deletes the only record of it*,
//! so a lease taken and dropped while you looked away leaves no trace. Lease
//! history therefore cannot be derived — it has to be logged.
//!
//! Deliberately kept as small as new persisted state can be:
//!   * ONE file, `.pact/events.jsonl`, already gitignored by the `.pact/` rule.
//!   * LEASE events only. Message events are derivable from bd and are NOT
//!     duplicated here — two sources of truth for one fact is worse than none.
//!   * [`append`] cannot fail the caller. A missing feed is an inconvenience;
//!     a lease acquire that failed because logging failed is a coordination bug.
//!   * Bounded: see [`MAX_LINES`].
//!   * Garbage lines are skipped, not fatal, exactly as `lease::list` skips
//!     unparsable lock files.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::repo::pact_dir;

/// Bounded growth. An unbounded log in a long-lived repo is a slow leak, and
/// nobody reads the 20000th-most-recent lease acquire. Strategy, kept dumb on
/// purpose: after an append, if the file exceeds `MAX_LINES`, rewrite it with
/// only the newest `KEEP_LINES`. No rotation, no sidecar files, no index. The
/// slack between the two constants is what stops every append past the cap from
/// rewriting the file; as written it rewrites once per `MAX_LINES - KEEP_LINES`
/// appends. At ~150 bytes a line the file stays under a megabyte.
const MAX_LINES: usize = 5000;
const KEEP_LINES: usize = 4000;

/// The chain point a chain-tracked line binds to when no preceding line has
/// one to offer — the first line ever tracked, or the first tracked line
/// after a run of untracked ones (pact-m7j.2.5). A plain string rather than
/// `Option::None` threaded through the mix function: it is itself an input to
/// the hash, and "no prior chain" has to hash to something stable, not to the
/// absence of a byte.
pub const CHAIN_GENESIS: &str = "genesis";

/// One lease-lifecycle event. `kind` is one of the strings emitted by
/// `lease.rs`: `"acquired"`, `"renewed"`, `"released"`, `"stolen"`,
/// `"displaced"`, `"force-released"`, `"expired"`, `"restored"`, `"refused"`.
/// Kept as a plain `String` rather than an enum so an older `pact` reading a
/// newer log shows an unknown kind instead of refusing to parse the line.
///
/// `"expired"` and `"displaced"` are the two kinds whose `agent` did not run the
/// command that wrote them; both belong to the holder whose claim ENDED, not to
/// whoever ended it (pact-rnc.13, pact-mqw.1). A lapsed lease is noticed by
/// whoever collects the lock; a displaced one is noticed by whoever stole it.
/// They are kept distinct because the difference is exactly what a reader needs:
/// `"expired"` means a TTL ran out and nobody was harmed, `"displaced"` means a
/// live claim was overridden via `--steal`. Both are immediately followed by a
/// `"stolen"` row under the incoming agent, so `owner_of` still resolves to the
/// new holder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// RFC3339.
    pub at: String,
    pub agent: String,
    pub kind: String,
    /// The leased path, for lease events.
    pub path: Option<String>,
    /// Free text: the lease note, the displaced holder, etc.
    pub detail: Option<String>,
    /// The TTL, in seconds, of the lease this event is about.
    ///
    /// `None` for events written before pact recorded it, which is why the field
    /// is optional rather than defaulted to the current constant: `pact audit`
    /// must be able to tell "this hold had a 900s TTL" from "this hold's TTL is
    /// unknown, assume the default of the era". Defaulting here would erase that
    /// distinction and make every historical hold look like it had whatever TTL
    /// the reading binary was compiled with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    /// `kind: "annotation"` only — the 1-based line numbers this annotation
    /// marks as not-real-history.
    ///
    /// The log is append-only and wrong entries are never removed, so a
    /// correction is a new entry that points at the old ones. See
    /// `audit::ANNOTATION_KIND` for why that is the right shape and
    /// docs/audit.md for the incident that produced the first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covers_lines: Option<Vec<usize>>,
    /// `kind: "annotation"` only — who is asserting the correction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// `kind: "force-released"` only — the agent whose live claim this event
    /// destroyed, when a name survived to report one.
    ///
    /// Unlike `expired` (see the struct doc above), a force-released event's
    /// own `agent` is the one who forced it, not the one displaced — so
    /// `audit::reconstruct` closing a hold with `open.remove(&e.agent)` found
    /// nothing under the forcer's name and left the real holder's window open
    /// indefinitely, counting the close as orphaned instead (pact-m7j.2.6).
    /// `None` when no holder name survived to report one — a corrupt lock
    /// force-removed, whose `existing.agent` was never readable — where that
    /// really is the correct, unimprovable outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displaced: Option<String>,
    /// A hash binding this event to the one physically before it in the file,
    /// so a hand-edited or forged line breaks the chain instead of reading
    /// identically to a pact-authored one.
    ///
    /// Strictly additive and opt-in-to-check (pact-m7j.2.5): `None` on every
    /// line written before this field existed — including this repository's
    /// own committed history — and `owner_of`/`actors`/`audit::summary` keep
    /// trusting every line exactly as they did before this existed. Changing
    /// what those consumers trust is the maintainer call the bead that asked
    /// for this explicitly flagged as unresolved; this field only feeds a new,
    /// separate `pact audit --check chain-integrity`, which never fails a line
    /// for lacking it, only counts it as predating the chain (or not written
    /// by pact).
    ///
    /// Computed by `append_bounded` as a hash of this event's own canonical
    /// JSON (itself serialized with this field cleared, so the hash never
    /// includes itself) mixed with the chain point it binds to: the nearest
    /// preceding line's `chain_hash` if that line parses and has one, or
    /// [`CHAIN_GENESIS`] otherwise. That "nearest preceding line" rule, not a
    /// running accumulator, is what lets a log with chain-tracked lines mixed
    /// among untracked ones (the shape every real repo has, forever, once this
    /// lands) verify cleanly: each tracked line only ever answers for the line
    /// immediately before it, never for history before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_hash: Option<String>,
    /// Which worktree of this repository pact was invoked from — a linked
    /// worktree's name, `"main"`, or `"outside"` (pact-ler.1). See
    /// [`crate::repo::invoked_from`].
    ///
    /// Stamped by [`append`] on **every** event, unconditionally, rather than
    /// by each call site or only when worktree topology is detected. That is
    /// the whole point of the field. `LeaseInfo`'s existing `branch`/
    /// `worktree` pair is gated on `RepoContext::has_worktrees` (so a repo
    /// that never uses worktrees keeps byte-identical lock files) — correct
    /// for a lock file, useless for a log, because a gated field cannot tell
    /// "not applicable" from "not recorded".
    ///
    /// Measured need: a 20-agent fleet run with one git worktree per agent
    /// (megablast, 2026-08-08) produced 62 events indistinguishable from a
    /// plain single-checkout run, because a lease event has never carried any
    /// topology at all. The one place it was recorded — the lock file — is
    /// deleted on release *and* gitignored, so the run's topology was
    /// unrecoverable by construction.
    ///
    /// `None` dates a line to a pact older than this field, exactly as
    /// `ttl_secs` and `chain_hash` already do for theirs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoked_from: Option<String>,
    /// The coordination scope actually in force — `"shared"` or `"local"`.
    /// See [`crate::repo::effective_scope`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The pact version that wrote this line, so a behaviour change can be
    /// dated against the log rather than guessed at from surrounding commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pact_version: Option<String>,
    /// `kind: "acquired"`/`"stolen"` only — the git blob id of the leased
    /// path's content at the moment it was claimed, or `None` when the file
    /// did not exist yet (a lease on a file you are about to create is a
    /// documented workflow, see docs/leases.md).
    ///
    /// Written with `git hash-object -w`, so the blob is retrievable
    /// afterwards and `pact watch`'s release-time diff can be computed against
    /// it. That matters more than it looks: the protocol now says commit
    /// before you release, so by release time the working tree is usually
    /// clean and `git diff HEAD` would show nothing at all. The at-acquire
    /// blob is the only fixed point that survives the holder committing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// `kind: "notified"`/`"watch-delivery-failed"` only — the subscriber the
    /// delivery was for. `agent` stays the releasing agent, matching the
    /// convention every other kind follows (the row belongs to whoever ran the
    /// command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscriber: Option<String>,
    /// `kind: "notified"` only — the bead id of the message that was sent, so
    /// a delivery can be followed to the thread it created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Which revision of the managed protocol block was in force in this
    /// repository when the event was written (pact-okz.1).
    ///
    /// Answers "which protocol were these agents following", which nothing
    /// could answer before: establishing it for three fleet runs took git
    /// archaeology on `src/agents_md.rs`, and getting it wrong produced a
    /// wrong conclusion about whether agents message voluntarily — 223
    /// messages cited as evidence all predated the change that suppressed
    /// them. See `agents_md::current_block_hash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_hash: Option<String>,
    /// The repository's `HEAD` when this hold opened or closed (pact-b73.3),
    /// short form. `None` on kinds that are not a hold boundary, and where
    /// git cannot answer — a repo with no commits yet.
    ///
    /// Exists because agent identity does not survive into git. Across three
    /// fleet runs every commit carried ONE git author (grimcast 90/90,
    /// megablast 62/62) while the agents were 23 and 20 distinct identities,
    /// so "did this agent commit during that agent's hold" — the question a
    /// coordination post-mortem most wants — could not be answered from git
    /// at all, and `--check commit-correlation` had to infer the binding from
    /// timestamps alone.
    ///
    /// An open and its matching close now bracket an exact commit range, so
    /// what an agent landed under a lease stops being an inference. pact
    /// already computed this value and threw it away: `head_short` existed
    /// only to name a commit in a truncated watch diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// On a `refused` row: who was holding the path, how much of their lease was
    /// left, and where they were holding it from (pact-1gv.1).
    ///
    /// Every one of these facts was already in the refusal — inside `detail`, in
    /// English. That made them unusable by anything except a human:
    ///
    /// ```text
    /// "held by agent-01 on branch crucible/agent-01 in worktree agent-01
    ///  (9s old, 591s remaining), use --steal to override — my note: ..."
    /// ```
    ///
    /// The trap is `ttl_secs`, which a reader naturally takes for the holder's
    /// remaining time and is not: it is the ttl the REFUSED agent asked for. In
    /// the crucible log it reads 600 on all 33 of agent-02's refusals of
    /// `src/eval.rs` while the holder's advertised remaining ranged 96–597s
    /// (median 355). An agent — or a check — that trusted `ttl_secs` to decide how
    /// long to wait would learn nothing, and agent-02 duly retried every 15
    /// seconds, 33 times, against a median 355s of remaining hold.
    ///
    /// So these are stamped from the same resolution that composes the prose, at
    /// the same moment, and the prose is unchanged. Two representations of one
    /// fact that cannot drift, rather than one representation only a regex can
    /// reach — the same reasoning `displaced` was added under (pact-m7j.2.6).
    ///
    /// `None` on every other kind, and on `refused` rows written before this
    /// shipped. Backfill is impossible, so absence means "not recorded", never
    /// zero — the discipline `chain_untracked` and `topology_unstamped` follow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    /// Seconds left on the holder's lease at the moment of the refusal. See
    /// [`Event::holder`] — this is the number `ttl_secs` is mistaken for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_remaining_secs: Option<i64>,
    /// The holder's branch, when the lock recorded one. Distinct from
    /// `invoked_from`, which is the REFUSED agent's own worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_branch: Option<String>,
    /// The holder's worktree, when the lock recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_worktree: Option<String>,
}

/// For appending: creates `.pact/` if needed.
fn events_file(repo_root: &Path) -> Result<PathBuf> {
    Ok(pact_dir(repo_root)?.join("events.jsonl"))
}

/// For reading: never creates anything (pact-rnc.27).
fn events_file_path(repo_root: &Path) -> PathBuf {
    crate::repo::pact_dir_path(repo_root).join("events.jsonl")
}

/// A temp filename no other writer will pick: pid AND thread id AND a
/// nanosecond stamp.
///
/// The thread id is the part that was missing and the part that matters. Two
/// threads in ONE process share a pid, so `tmp-{pid}` collides whenever the
/// clock repeats — which it does under load on a coarse clock. In `lease.rs`
/// that produced an intermittently red concurrency test: both threads wrote one
/// temp file, one renamed it into place, the other's rename hit ENOENT and
/// reported failure for a lease that had in fact been written (fixed in
/// edd0eb2). `events.rs` carried the identical `tmp-{pid}` form and was simply
/// not audited at the same time — reachable from the same place, because the
/// lease tests spawn threads that call `acquire` -> `log_event` -> `append`.
///
/// It matters more here than for a lock file: releasing a lease deletes the
/// only record of it, so this log is the sole history, and `pact agents --for`
/// and `msg send --to-owner-of` now read from it. A truncated log loses
/// ownership silently.
///
/// One function so the two atomic-write sites cannot drift apart again.
pub fn unique_temp_name(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{prefix}-{}-{:?}-{nanos}",
        std::process::id(),
        std::thread::current().id()
    )
}

/// Does `path` exist, have nonzero length, and NOT already end in `\n`?
///
/// A `true` here means the last append into this file was torn: it wrote part
/// of a line and then failed (ENOSPC/EIO/EDQUOT — `writeln!`'s error is
/// swallowed by [`append`]'s infallible signature, so the file is the only
/// place this shows up). The next append must not simply continue writing at
/// that dangling offset, or its well-formed line glues onto the torn prefix
/// and BOTH become one unparseable line — losing the new event along with the
/// old one. A missing or empty file is not torn; there is nothing to sever.
fn ends_without_newline(path: &Path) -> Result<bool> {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e).with_context(|| format!("reading metadata for {}", path.display()))
        }
    };
    if len == 0 {
        return Ok(false);
    }
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    f.seek(SeekFrom::End(-1))
        .with_context(|| format!("seeking {}", path.display()))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)
        .with_context(|| format!("reading last byte of {}", path.display()))?;
    Ok(last[0] != b'\n')
}

/// Non-cryptographic FNV-1a 64-bit mix of a chain point and one event's
/// canonical JSON, hex-encoded. Same algorithm as `msg.rs`'s `fnv1a64` (see
/// its doc comment for why FNV-1a and not `DefaultHasher` or a crypto hash:
/// deterministic across runs, no dependency, and the threat model this and
/// that hash both accept is naive/accidental collision, not an adversary
/// searching for one). Duplicated rather than imported: `msg.rs`'s version is
/// private to that module and mixes a different shape of input for a
/// different purpose (message dedup, not log tamper-evidence); sharing it
/// would couple two unrelated subsystems for a dozen lines of arithmetic.
pub fn chain_hash_of(prev_chain_point: &str, event_json_without_chain_hash: &str) -> String {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for part in [prev_chain_point, event_json_without_chain_hash] {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // Separator between the two parts, same reasoning as `msg.rs`: without
        // it, a chain point and a JSON body that concatenate to the same bytes
        // as a different pair would hash identically.
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// What `ev`'s `chain_hash` should be, given the chain point it binds to.
///
/// The ONE place this computation happens — [`append_bounded`] calls it to
/// write a new line's hash, and `audit::run_check`'s chain-integrity check
/// (via [`verify_chain`]) calls it again to recompute what an existing line's
/// hash should have been. Two implementations of this one rule could drift
/// apart silently; one function cannot (pact-m7j.2.5).
fn expected_chain_hash(prev_chain_point: &str, ev: &Event) -> Result<String> {
    // Cleared, not merely ignored: `ev` may already carry a `chain_hash` (an
    // event read back from the log, in `verify_chain`'s case), and hashing
    // over a value that includes itself would make the hash a function of
    // whatever happened to be there rather than of the event's real content.
    let mut cleared = ev.clone();
    cleared.chain_hash = None;
    let canonical = serde_json::to_string(&cleared)?;
    Ok(chain_hash_of(prev_chain_point, &canonical))
}

/// The chain point the NEXT append should bind to: the nearest line at the
/// end of the file that parses as an `Event`, and that line's own
/// `chain_hash` if it has one, or [`CHAIN_GENESIS`] if it does not (an empty
/// file, one with no chain-tracked lines yet, or whose most recent parseable
/// line predates chain tracking).
///
/// Deliberately does NOT search further back past that nearest parseable line
/// for an earlier line that DOES have a `chain_hash`: a torn tail or garbage
/// line is skipped (it never parses), but a well-formed, untracked line
/// resets the chain to [`CHAIN_GENESIS`] for whatever comes after it. That
/// matches how a mixed-age log actually reads: an untracked line is real
/// history this feature knows nothing about, not a gap to see through.
fn last_chain_point(path: &Path) -> Result<String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CHAIN_GENESIS.to_string()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(contents
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Event>(l).ok())
        .and_then(|e| e.chain_hash)
        .unwrap_or_else(|| CHAIN_GENESIS.to_string()))
}

/// One line whose `chain_hash` does not match what [`expected_chain_hash`]
/// says it should be, given the chain point the line before it offers.
#[derive(Debug, Clone, Serialize)]
pub struct ChainMismatch {
    pub line: usize,
    pub agent: String,
    pub kind: String,
    pub at: String,
    pub expected: String,
    pub found: String,
}

/// Walk the whole numbered log and check every chain-tracked line against
/// [`expected_chain_hash`], returning the mismatches plus how many lines were
/// tracked at all versus not.
///
/// Takes the RAW numbered log — `audit::load`'s annotation-filtered,
/// `--since`-narrowed view is the wrong input here: the chain is a property of
/// PHYSICAL line adjacency in the file as written, and an annotation line is
/// still a real physical entry the writer's hash chain ran through, whatever
/// a lease-history statistic later decides to exclude it from.
///
/// A line with no `chain_hash` is counted in `untracked` and otherwise
/// ignored — not flagged, not skipped when it comes to being the reference
/// point for the line after it (see [`last_chain_point`]'s doc comment for why
/// that "reset to genesis" rule is right). This is what keeps a log with NO
/// chain-tracked lines anywhere — every log that predates pact-m7j.2.5,
/// including this repository's own committed history — reporting cleanly:
/// zero tracked, zero mismatches, never "tampered".
pub fn verify_chain(numbered: &[(usize, Event)]) -> (Vec<ChainMismatch>, usize, usize) {
    let mut mismatches = Vec::new();
    let mut tracked = 0usize;
    let mut untracked = 0usize;
    // `None` here means "the nearest preceding line parsed but had no
    // chain_hash", i.e. CHAIN_GENESIS — matching `last_chain_point`'s
    // write-time rule exactly, one line at a time as we walk forward.
    let mut chain_point: Option<String> = None;
    for (line, e) in numbered {
        match &e.chain_hash {
            Some(actual) => {
                tracked += 1;
                let prev = chain_point.as_deref().unwrap_or(CHAIN_GENESIS);
                let expected = expected_chain_hash(prev, e).unwrap_or_default();
                if &expected != actual {
                    mismatches.push(ChainMismatch {
                        line: *line,
                        agent: e.agent.clone(),
                        kind: e.kind.clone(),
                        at: e.at.clone(),
                        expected,
                        found: actual.clone(),
                    });
                }
                chain_point = Some(actual.clone());
            }
            None => {
                untracked += 1;
                chain_point = None;
            }
        }
    }
    (mismatches, tracked, untracked)
}

/// Fill in the invocation context every event carries (pact-ler.1).
///
/// Overwrites rather than filling only when absent: the caller constructing
/// an `Event` does not know where pact was invoked from, and a value it
/// somehow supplied would be a guess overriding a measurement. The one
/// exception that matters — re-appending an event read back from a log —
/// does not exist in pact, which never rewrites history.
fn stamp_context(repo_root: &Path, ev: &mut Event) {
    let ctx = crate::repo::RepoContext::resolve(repo_root);
    ev.invoked_from = Some(crate::repo::invoked_from(&ctx));
    ev.scope = Some(crate::repo::effective_scope().to_string());
    ev.pact_version = Some(env!("CARGO_PKG_VERSION").to_string());
    // The protocol the agents in this run were actually reading — the block in
    // AGENTS.md, not the one this binary would write. `pact_version` above
    // already says which binary ran; a repo that has not re-run `pact init`
    // since an upgrade is still following the older text, and that difference
    // is exactly what makes a before/after comparison across a protocol change
    // interpretable. `None` when there is no readable managed block.
    ev.protocol_hash = crate::agents_md::current_block_hash(repo_root);
    // The commit this repository was on when the hold opened or closed
    // (pact-b73.3). Gated on kind, unlike everything above it, and the
    // asymmetry is deliberate rather than an oversight:
    //
    // * `invoked_from`/`scope`/`pact_version` are meaningful for EVERY event —
    //   where the command ran, under what rules, from which binary — so a
    //   gated field there could not tell "not applicable" from "not recorded".
    // * `head` is meaningful only at a hold's boundaries. A `notified` or
    //   `watched` event has a HEAD, but it answers nothing, and stamping it
    //   would spawn a `git rev-parse` per delivery — 87 of them in the run
    //   that motivated this — to record noise.
    //
    // `expired` is excluded with the open/close kinds it otherwise resembles,
    // for the same reason expiry delivers no watch diff: the holder is gone,
    // and HEAD at collection time belongs to whoever swept the lock.
    if matches!(
        ev.kind.as_str(),
        "acquired" | "stolen" | "released" | "force-released"
    ) {
        ev.head = crate::git_history::head_short(repo_root);
    }
}

/// Append one event to `.pact/events.jsonl`.
///
/// Infallible by signature: I/O errors are swallowed, because a logging
/// failure must never break the lease operation that triggered it.
pub fn append(repo_root: &Path, ev: &Event) {
    let _ = append_bounded(repo_root, ev, MAX_LINES, KEEP_LINES);
}

/// The fallible body of [`append`], with the cap injected so tests don't have
/// to write 5000 lines to exercise trimming.
///
/// Locking here is shared/exclusive, not "trim-only": a plain append takes a
/// cheap **shared** lock (any number of appenders hold it at once, so they
/// never serialize against each other — the common case still pays only one
/// extra `flock` syscall, not a queue), and the trim's read-modify-rename
/// takes an **exclusive** one. A first pass guarded only the trim branch, on
/// the theory that `O_APPEND` writes need no coordination at all — true for
/// two plain appends racing each other, but not for a plain append racing a
/// CONCURRENT trim's rename: a write landing in the gap between the trim's
/// fresh read and its rename goes to the inode the rename is about to orphan,
/// and is silently gone once the rename swaps the directory entry to the
/// freshly-written one. A test with real concurrent writers reproduced that
/// loss reliably even with the trim-only guard, which is what the shared lock
/// on the plain append actually closes.
fn append_bounded(repo_root: &Path, ev: &Event, max_lines: usize, keep_lines: usize) -> Result<()> {
    let path = events_file(repo_root)?;

    {
        // Shared: blocks only while a trim (below) holds the exclusive lock,
        // and does not serialize against other appenders holding this same
        // shared lock.
        let _guard = EventsFileLock::acquire_shared(&path)?;
        // The torn-tail check happens under the lock too, so it never reads a
        // file mid-rewrite by a concurrent trim.
        let sever_torn_tail = ends_without_newline(&path)?;
        // ponytail: read under the SHARED lock, so this still races another
        // appender that is also (legitimately) holding a shared lock at the
        // same instant — shared locks don't serialize against each other by
        // design, see above. Two truly simultaneous appends can each read the
        // same prior chain point and both chain from it, which
        // `audit::verify_chain` then reads as a broken link on one of them
        // even though both writes are genuine. Closing it means computing the
        // chain point under the EXCLUSIVE lock instead, which would serialize
        // every append and undo the throughput property the shared lock above
        // exists to provide — not a trade worth making for an
        // informational-only check (pact-m7j.2.5) against a race that needs
        // two writers hitting the very same repo in the very same instant,
        // which real pact usage does not do (lease ops run at agent speed,
        // not in a tight loop). Upgrade path if that ever stops being true:
        // fold this read into the exclusive-lock section below.
        let prev_chain_point = last_chain_point(&path)?;
        let mut chained = ev.clone();
        // Stamped HERE, in the one funnel every event of every kind passes
        // through, and before the chain hash is computed (pact-ler.1).
        //
        // Not at the call sites: `log_event` is the common path but not the
        // only one — `release_fs`'s force-release bypasses it to set
        // `displaced`, and any future kind is one more place to forget. A
        // field documented as unconditional has to be unforgettable, and a
        // per-call-site stamp is exactly how `branch`/`worktree` ended up
        // conditional in the first place.
        stamp_context(repo_root, &mut chained);
        // Over `chained`, not `ev`: the context fields are part of what the
        // chain attests to, so a forged line cannot strip or rewrite them and
        // still verify.
        chained.chain_hash = Some(expected_chain_hash(&prev_chain_point, &chained)?);
        let line = serde_json::to_string(&chained)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // One `write_all` call, not `writeln!` (which would emit the line and
        // the trailing newline as two separate `write_str`/`write` calls): a
        // shared lock deliberately allows other appenders to hold it at the
        // same time, and `O_APPEND` only makes a SINGLE `write(2)` atomic with
        // respect to them — two separate writes for one logical line leaves a
        // window where a concurrent appender's write lands between them,
        // gluing two events into one unparseable line with no lock able to
        // prevent it. A leading `\n` (severing a torn tail from a prior
        // failed write, see `ends_without_newline`) is folded into the same
        // buffer for the same reason.
        let mut buf = String::with_capacity(line.len() + 2);
        if sever_torn_tail {
            buf.push('\n');
        }
        buf.push_str(&line);
        buf.push('\n');
        f.write_all(buf.as_bytes())
            .with_context(|| format!("appending to {}", path.display()))?;
    }

    // Reading the file back on every append is a few hundred microseconds at
    // this cap, and lease operations happen at agent speed, not in a loop.
    // This is a cheap, UNLOCKED read purely to decide whether trimming is
    // worth even trying — the exclusive lock below is only taken when it is.
    let contents = std::fs::read_to_string(&path)?;
    if contents.lines().count() > max_lines {
        // Two writers racing this branch used to each snapshot the file, trim
        // their own stale copy, and rename over each other — the loser's
        // rename replaced the directory entry with a file built before the
        // winner's event existed, discarding it with no error and no signal.
        // The exclusive lock makes the two rewrites mutually exclusive, and
        // also excludes any plain append from landing mid-rewrite (see the
        // shared lock above).
        let _guard = EventsFileLock::acquire_exclusive(&path)?;
        // Re-read now that we hold the lock: any append that landed between
        // the unlocked read above and here must be picked up before we decide
        // what to keep.
        let fresh = std::fs::read_to_string(&path)?;
        let fresh_lines = fresh.lines().count();
        if fresh_lines > max_lines {
            let kept: Vec<&str> = fresh
                .lines()
                .skip(fresh_lines.saturating_sub(keep_lines))
                .collect();
            // Rewrite via temp + rename so a reader never sees a half-trimmed
            // file.
            let tmp = path.with_file_name(unique_temp_name("events.jsonl.tmp"));
            std::fs::write(&tmp, kept.join("\n") + "\n")?;
            std::fs::rename(&tmp, &path)?;
        }
        // else: a concurrent trim (seen in the fresh read) already brought the
        // file back under the cap; nothing left to do.
    }
    Ok(())
}

/// Guards `append_bounded`'s write path against its own trim: shared while
/// appending, exclusive while trimming, so the two can never interleave.
///
/// A dedicated sidecar file (`events.jsonl.trimlock`), never `events.jsonl`
/// itself: `flock` exclusivity is per-inode, and the trim branch replaces
/// `events.jsonl`'s inode via `rename`, so locking the path being renamed
/// would stop protecting anything the instant the rename happened — a holder
/// with the old inode open and a newcomer who just opened the new one would
/// hold unrelated locks. The sidecar's inode never changes, so every caller's
/// lock is always on the same target.
///
/// Deliberately NOT `lease.rs`'s `WriteGuard`, despite being the same
/// `flock(2)` pattern: importing it here would couple two unrelated
/// subsystems for the sake of ~15 lines of FFI, and this module already
/// reaches for a bare `extern "C"` block rather than a dependency for less
/// than this. The lock file is never deleted, for the identical reason
/// `WriteGuard`'s guard file never is (see its doc comment in lease.rs):
/// unlinking a live flock target while another waiter might be about to open
/// that same name reopens the exact race this exists to prevent.
struct EventsFileLock {
    _file: std::fs::File,
}

#[cfg(unix)]
mod events_lock_ffi {
    use std::os::unix::io::RawFd;

    extern "C" {
        fn flock(fd: RawFd, operation: i32) -> i32;
    }
    pub const LOCK_SH: i32 = 1;
    pub const LOCK_EX: i32 = 2;

    /// Blocks until the requested lock (`LOCK_SH` or `LOCK_EX`) is granted.
    /// No timeout and no staleness reclaim, matching `WriteGuard`: every
    /// critical section held here is a handful of syscalls, and `flock`
    /// releases automatically the instant a holder's process exits or
    /// crashes, so there is no "is the holder actually dead" question to
    /// answer with a guess.
    pub fn lock(fd: RawFd, operation: i32) -> std::io::Result<()> {
        if unsafe { flock(fd, operation) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

impl EventsFileLock {
    #[cfg(unix)]
    fn acquire(events_path: &Path, operation: i32) -> Result<Self> {
        let mut lock_name = events_path.as_os_str().to_owned();
        lock_name.push(".trimlock");
        let path = PathBuf::from(lock_name);
        // Never `create_new`: every writer opens this SAME file concurrently
        // as the normal case — `flock`, not file creation, is what provides
        // exclusivity. Never truncated or removed either; see the struct doc
        // comment.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening events lock {}", path.display()))?;
        use std::os::unix::io::AsRawFd;
        events_lock_ffi::lock(file.as_raw_fd(), operation)
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self { _file: file })
    }

    #[cfg(unix)]
    fn acquire_shared(events_path: &Path) -> Result<Self> {
        Self::acquire(events_path, events_lock_ffi::LOCK_SH)
    }

    #[cfg(unix)]
    fn acquire_exclusive(events_path: &Path) -> Result<Self> {
        Self::acquire(events_path, events_lock_ffi::LOCK_EX)
    }

    #[cfg(not(unix))]
    fn acquire_shared(_events_path: &Path) -> Result<Self> {
        compile_error!("EventsFileLock requires flock(2); pact does not build on non-Unix");
    }

    #[cfg(not(unix))]
    fn acquire_exclusive(_events_path: &Path) -> Result<Self> {
        compile_error!("EventsFileLock requires flock(2); pact does not build on non-Unix");
    }
}

/// The most recent lease events, oldest-first (so a feed reads top-to-bottom
/// like a log), at most `limit`. A missing file is an empty feed, not an error.
/// Unparsable lines are skipped.
pub fn recent(repo_root: &Path, limit: usize) -> Result<Vec<Event>> {
    let path = events_file_path(repo_root);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let events: Vec<Event> = contents
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let start = events.len().saturating_sub(limit);
    Ok(events[start..].to_vec())
}

/// Who last touched a path, and how their claim on it ended.
///
/// pact models who is HOLDING a path right now and, until this existed, nothing
/// else. Nobody could ask "who is this file's agent", "who do I send this defect
/// to", or "who touched it last". In one nine-agent run that cost three separate
/// failures: `src/doctor.rs` blocked two agents in sequence because nobody held
/// it, so `lease ls` showed it exactly like an untouched file; one word-fix was
/// routed to the same agent by three others and then nearly applied twice; and
/// 51 of 59 messages were never read, because they were addressed to processes
/// that had exited rather than to the work.
///
/// Derived, never stored — the answer was always in `.pact/events.jsonl`, it
/// just took hand-scanning `pact log` to get it. No registry, no new state, and
/// nothing to keep in sync (see docs/architecture.md).
#[derive(Debug, Clone)]
pub struct Owner {
    pub agent: String,
    /// RFC3339 of the last event on this path.
    pub at: String,
    /// `acquired` / `released` / `renewed` / `expired` / `stolen`.
    pub kind: String,
    /// The lease note that came with it, when there was one.
    pub detail: Option<String>,
}

/// Does this kind mean its `agent` actually had custody of the path?
///
/// Ownership questions — `pact agents --for <path>`, `lease acquire`'s
/// prior-claim note, `lease ls --all`'s free-but-owned rows, and above all
/// `msg send --to-owner-of <path>` — must not answer with an agent that never
/// held the file. Before this, every one of them took the last event on a path
/// whatever it said, which was already wrong for `refused` (logged under the
/// agent who was DENIED the lease, so a refusal made the loser look like the
/// owner and `--to-owner-of` addressed the wrong agent) and would have become
/// wrong again for every `watch` kind.
///
/// Listed as an allowlist, not a denylist, so a future kind is excluded until
/// somebody decides it means custody — the safe direction, because the failure
/// mode of guessing wrong here is misdirected mail.
pub fn is_custody(kind: &str) -> bool {
    matches!(
        kind,
        "acquired" | "stolen" | "renewed" | "released" | "force-released" | "expired" | "restored"
    )
}

/// The last agent to actually hold `path`, or `None` if pact has never seen it.
///
/// The log is bounded (rewritten to the newest 4000 lines past 5000), so an
/// owner is forgettable by design: a path nobody has touched in thousands of
/// events has no current owner, which is the honest answer.
pub fn owner_of(repo_root: &Path, path: &str) -> Result<Option<Owner>> {
    Ok(all(repo_root)?
        .into_iter()
        .rev()
        .filter(|e| is_custody(&e.kind))
        .find(|e| e.path.as_deref() == Some(path))
        .map(|e| Owner {
            agent: e.agent,
            at: e.at,
            kind: e.kind,
            detail: e.detail,
        }))
}

/// The last custody event for `path` authored by `agent` — "what did *I* last do
/// to this file", where [`owner_of`] answers "who has it now".
///
/// `lease release` needs the narrower question (pact-mqw.7). A lease that lapsed
/// and had its lock collected leaves no lock file, so release cannot tell that
/// case from "nothing was ever held here" by looking at the filesystem — and it
/// reported both as a clean success. The log knows, but only the asking agent's
/// own row answers it: a peer's expiry on the same path says nothing about
/// whether *this* agent overran its TTL.
///
/// Same bounded-log caveat as `owner_of`: past the rewrite horizon the answer is
/// `None`, which reads as "no expiry on record" rather than as a claim that none
/// happened.
pub fn last_custody_by(repo_root: &Path, path: &str, agent: &str) -> Result<Option<Owner>> {
    Ok(all(repo_root)?
        .into_iter()
        .rev()
        .filter(|e| is_custody(&e.kind))
        .find(|e| e.path.as_deref() == Some(path) && e.agent == agent)
        .map(|e| Owner {
            agent: e.agent,
            at: e.at,
            kind: e.kind,
            detail: e.detail,
        }))
}

/// What the last agent to RELEASE `path` left behind: their name, when, and the
/// content hash of the file at that moment.
///
/// The pairing this exists for (pact-mqw.3): a lease is exclusive in TIME, but
/// under the branch-per-agent worktree topology it is not exclusive across
/// COPIES. Agent A acquires, edits, commits to branch A, releases — compliant.
/// Agent B acquires and edits a different copy on branch B that never contained
/// A's change, commits, releases — also compliant. Both leases were honoured and
/// the conflict is deferred to a merge nobody holds a lease for.
///
/// The hash a releasing agent left is the only fixed point that can detect it, so
/// comparing it against the acquiring worktree's own hash answers "am I about to
/// edit a stale copy" at the one moment it is cheap to act on.
///
/// `None` when the log has no close for this path, when the closing event predates
/// content-hash stamping on releases, or when hashing failed at release time.
/// Every one of those is "cannot tell", which must read as silence rather than as
/// a warning — a false alarm on every first acquire would train agents to ignore
/// the real one.
pub fn last_released_content(repo_root: &Path, path: &str) -> Result<Option<ReleasedContent>> {
    Ok(all(repo_root)?
        .into_iter()
        .rev()
        .filter(|e| matches!(e.kind.as_str(), "released" | "force-released"))
        .find(|e| e.path.as_deref() == Some(path) && e.content_hash.is_some())
        .map(|e| ReleasedContent {
            agent: e.agent,
            at: e.at,
            hash: e.content_hash.unwrap_or_default(),
        }))
}

/// The three facts [`last_released_content`] answers with.
#[derive(Debug, Clone)]
pub struct ReleasedContent {
    pub agent: String,
    /// RFC3339 of the release.
    pub at: String,
    /// The path's git blob id as the releasing agent left it.
    pub hash: String,
}

/// Every path pact has ever seen an event for, with its last owner, most
/// recently touched first. Backs the free-but-owned rows in `lease ls --all`.
pub fn owners(repo_root: &Path) -> Result<Vec<(String, Owner)>> {
    let mut seen: Vec<(String, Owner)> = Vec::new();
    for e in all(repo_root)?.into_iter().rev() {
        // Same rule as `owner_of`: a subscription or a refusal is not custody.
        if !is_custody(&e.kind) {
            continue;
        }
        let Some(path) = e.path.clone() else { continue };
        if seen.iter().any(|(p, _)| *p == path) {
            continue;
        }
        seen.push((
            path,
            Owner {
                agent: e.agent,
                at: e.at,
                kind: e.kind,
                detail: e.detail,
            },
        ));
    }
    Ok(seen)
}

/// Every agent the log has ever seen act, with the timestamp of its most
/// recent event and how many events it produced.
///
/// `pact agents` used to build its roster from live lock files plus message
/// traffic, so an agent that acquired a lease, did the work and released it —
/// the correct behaviour — vanished the moment its last lock was deleted.
/// `msg send` then warned "no agent named X has acted in this repo" one line
/// after the resolver said "last seen 0s ago" (pact-6sx).
pub fn actors(repo_root: &Path) -> Result<Vec<(String, String, usize)>> {
    let mut seen: Vec<(String, String, usize)> = Vec::new();
    for e in all(repo_root)? {
        match seen.iter_mut().find(|(a, _, _)| *a == e.agent) {
            Some(row) => {
                // Oldest-first, so a later event is always the more recent.
                row.1 = e.at;
                row.2 += 1;
            }
            None => seen.push((e.agent, e.at, 1)),
        }
    }
    Ok(seen)
}

/// The whole log, oldest-first. `recent` is this truncated to a limit.
fn all(repo_root: &Path) -> Result<Vec<Event>> {
    Ok(numbered(repo_root)?.0.into_iter().map(|(_, e)| e).collect())
}

/// Every event paired with its 1-based line number, plus how many lines could not
/// be parsed at all.
///
/// The line number **is** the event id. Events carry no identifier of their own,
/// and inventing one would mean rewriting a log whose only virtue is being
/// append-only — whereas "line 47 of .pact/events.jsonl" is stable for a given
/// file and a human can go and look at it.
///
/// Unparseable lines are counted rather than discarded silently. An append-only
/// log gets cut mid-write, so a truncated final line is expected rather than
/// corrupt; `pact audit` reports the count so a reader can tell "the log has a
/// torn tail" from "the log is full of junk".
pub(crate) fn numbered(repo_root: &Path) -> Result<(Vec<(usize, Event)>, usize)> {
    let path = events_file_path(repo_root);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut out = Vec::new();
    let mut skipped = 0;
    for (i, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(line) {
            Ok(e) => out.push((i + 1, e)),
            Err(_) => skipped += 1,
        }
    }
    Ok((out, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, path: &str) -> Event {
        Event {
            at: chrono::Utc::now().to_rfc3339(),
            agent: "agent-a".into(),
            kind: kind.into(),
            path: Some(path.into()),
            detail: None,
            ttl_secs: None,
            // Lease events never annotate; only a hand-written
            // correction does. See audit::ANNOTATION_KIND.
            covers_lines: None,
            actor: None,
            displaced: None,
            // append() computes this; a hand-built fixture leaves it unset,
            // same as every event this test module writes before append().
            chain_hash: None,
            invoked_from: None,
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
        }
    }

    #[test]
    fn recent_creates_nothing_on_a_repo_that_has_never_used_pact() {
        // pact-rnc.27: reading the feed is a question. It must not leave a
        // `.pact/` behind, and a missing one means "no events", not an error.
        let tmp = tempfile::tempdir().unwrap();
        assert!(recent(tmp.path(), 10).unwrap().is_empty());
        assert!(
            !tmp.path().join(".pact").exists(),
            "a pure read created .pact/"
        );
    }

    #[test]
    fn append_then_recent_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &ev("acquired", "src/a.rs"));

        let got = recent(tmp.path(), 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "acquired");
        assert_eq!(got[0].path.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn recent_on_a_missing_feed_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(recent(tmp.path(), 10).unwrap().is_empty());
    }

    #[test]
    fn recent_returns_the_newest_limit_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5 {
            append(tmp.path(), &ev("acquired", &format!("f{i}.rs")));
        }

        let paths: Vec<String> = recent(tmp.path(), 3)
            .unwrap()
            .into_iter()
            .map(|e| e.path.unwrap())
            .collect();
        assert_eq!(
            paths,
            vec!["f2.rs", "f3.rs", "f4.rs"],
            "newest 3, oldest first"
        );
        assert_eq!(recent(tmp.path(), 0).unwrap().len(), 0);
        assert_eq!(
            recent(tmp.path(), 99).unwrap().len(),
            5,
            "limit above len is fine"
        );
    }

    #[test]
    fn a_corrupt_line_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &ev("acquired", "good1.rs"));
        // A partial write or a hand-edit: half a line, then valid JSON that
        // isn't an Event, then a blank line.
        let file = events_file(tmp.path()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(f, "{{\"at\":\"2026-07-31T00:00").unwrap();
        writeln!(f, "{{\"unrelated\":true}}").unwrap();
        writeln!(f).unwrap();
        drop(f);
        append(tmp.path(), &ev("released", "good2.rs"));

        let paths: Vec<String> = recent(tmp.path(), 10)
            .unwrap()
            .into_iter()
            .map(|e| e.path.unwrap())
            .collect();
        assert_eq!(paths, vec!["good1.rs", "good2.rs"]);
    }

    #[test]
    fn append_on_an_unwritable_repo_root_is_silent() {
        // repo_root is a *file*, so `.pact/` can't be created.
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = tmp.path().join("regular-file");
        std::fs::write(&not_a_dir, "x").unwrap();

        append(&not_a_dir, &ev("acquired", "src/a.rs")); // must not panic
        assert!(
            recent(&not_a_dir, 10).is_err(),
            "reading it still reports why"
        );
    }

    /// A prior failed `write_all` (ENOSPC/EIO/mid-write crash) leaves the file
    /// ending in a torn line with no trailing newline. The next append must
    /// not glue its own well-formed line onto that dangling prefix — that
    /// turns ONE physical line into unparseable garbage, losing the new event
    /// along with the old one. Fixture built directly, no fault injection
    /// needed: this is exactly the on-disk shape a torn `write_all` leaves.
    #[test]
    fn append_severs_a_torn_tail_instead_of_gluing_onto_it() {
        let tmp = tempfile::tempdir().unwrap();
        let file = events_file(tmp.path()).unwrap();
        std::fs::write(
            &file,
            r#"{"at":"2026-08-01T00:00:00Z","agent":"a","kind":"acq"#,
        )
        .unwrap();

        append(tmp.path(), &ev("released", "good.rs"));

        let got = recent(tmp.path(), 10).unwrap();
        assert_eq!(
            got.len(),
            1,
            "the new event must be intact and separately parseable, not glued to the torn prefix"
        );
        assert_eq!(got[0].path.as_deref(), Some("good.rs"));
        assert_eq!(got[0].kind, "released");
    }

    /// Two-plus writers racing `append_bounded`'s trim branch used to each
    /// snapshot the file, trim their own stale copy, and rename over each
    /// other — the loser's rename replaced the directory entry with a file
    /// built before the winner's event existed, discarding it with no error.
    ///
    /// `keep_lines` is set larger than the total number of events this test
    /// will ever write, so trimming never has a *legitimate* reason to drop
    /// anything: any event missing at the end is lost to the race, not to the
    /// bounded-log policy exercised by `trimming_caps_the_file`. `max_lines`
    /// is set tiny so nearly every append enters the guarded branch.
    #[test]
    fn concurrent_trims_never_lose_an_appended_event() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        const THREADS: usize = 4;
        const PER_THREAD: usize = 40;
        let max_lines = 2;
        let keep_lines = THREADS * PER_THREAD + 10;

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let path = format!("t{t}-{i}.rs");
                        append_bounded(&root, &ev("acquired", &path), max_lines, keep_lines)
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let seen: std::collections::HashSet<String> = recent(&root, THREADS * PER_THREAD + 10)
            .unwrap()
            .into_iter()
            .filter_map(|e| e.path)
            .collect();

        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                let path = format!("t{t}-{i}.rs");
                assert!(
                    seen.contains(&path),
                    "event for {path} was lost to a concurrent trim-rename race"
                );
            }
        }
    }

    #[test]
    fn trimming_caps_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..12 {
            append_bounded(tmp.path(), &ev("acquired", &format!("f{i}.rs")), 10, 6).unwrap();
        }

        let lines = std::fs::read_to_string(events_file(tmp.path()).unwrap())
            .unwrap()
            .lines()
            .count();
        assert!(lines <= 10, "file stays under the cap, got {lines} lines");
        // Trimming keeps the newest, and the file is still parseable after it.
        let got = recent(tmp.path(), 100).unwrap();
        assert_eq!(got.last().unwrap().path.as_deref(), Some("f11.rs"));
        assert_eq!(got.len(), lines);
    }

    // --------------------------------------------------------- chain hashing

    #[test]
    fn the_first_appended_event_ever_chains_from_genesis() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &ev("acquired", "a.rs"));

        let got = recent(tmp.path(), 1).unwrap();
        let mut without_hash = got[0].clone();
        without_hash.chain_hash = None;
        assert_eq!(
            got[0].chain_hash.as_deref(),
            Some(
                expected_chain_hash(CHAIN_GENESIS, &without_hash)
                    .unwrap()
                    .as_str()
            ),
            "the very first line has nothing to chain from but genesis"
        );
    }

    #[test]
    fn a_second_appended_event_chains_from_the_first_ones_hash() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &ev("acquired", "a.rs"));
        append(tmp.path(), &ev("released", "a.rs"));

        let got = recent(tmp.path(), 2).unwrap();
        let first_hash = got[0].chain_hash.clone().expect("first line is tracked");
        let mut second_without_hash = got[1].clone();
        second_without_hash.chain_hash = None;
        assert_eq!(
            got[1].chain_hash.as_deref(),
            Some(
                expected_chain_hash(&first_hash, &second_without_hash)
                    .unwrap()
                    .as_str()
            ),
            "the second line must bind to the FIRST line's hash, not to genesis again"
        );
    }

    /// A line written before this feature existed has no `chain_hash`. The
    /// next append must not treat that gap as if there were nothing before
    /// it — genesis on purpose, per `last_chain_point`'s doc comment, but a
    /// FRESH genesis for this new run, not a silent reuse of the old line's
    /// (nonexistent) hash.
    #[test]
    fn an_append_after_a_pre_chain_tracking_line_restarts_from_genesis() {
        let tmp = tempfile::tempdir().unwrap();
        let file = events_file(tmp.path()).unwrap();
        std::fs::write(
            &file,
            format!(
                "{}\n",
                serde_json::to_string(&ev("acquired", "old.rs")).unwrap()
            ),
        )
        .unwrap();

        append(tmp.path(), &ev("released", "old.rs"));

        let got = recent(tmp.path(), 2).unwrap();
        assert!(
            got[0].chain_hash.is_none(),
            "the pre-existing line is untouched"
        );
        let mut without_hash = got[1].clone();
        without_hash.chain_hash = None;
        assert_eq!(
            got[1].chain_hash.as_deref(),
            Some(
                expected_chain_hash(CHAIN_GENESIS, &without_hash)
                    .unwrap()
                    .as_str()
            )
        );
    }

    #[test]
    fn verify_chain_reports_zero_tracked_on_a_log_with_no_chain_hash_anywhere() {
        let numbered = vec![(1, ev("acquired", "a.rs")), (2, ev("released", "a.rs"))];
        let (mismatches, tracked, untracked) = verify_chain(&numbered);
        assert!(mismatches.is_empty());
        assert_eq!(tracked, 0);
        assert_eq!(untracked, 2);
    }

    /// The property the whole feature exists for: a line whose recorded
    /// `chain_hash` does not match what it should be, given the line before
    /// it, must be reported — and reported ALONE, not the untampered line
    /// next to it.
    #[test]
    fn verify_chain_flags_a_hand_edited_chain_hash_and_only_that_line() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &ev("acquired", "a.rs"));
        append(tmp.path(), &ev("released", "a.rs"));
        let (mut all, unparseable) = numbered(tmp.path()).unwrap();
        assert_eq!(unparseable, 0);
        all[1].1.chain_hash = Some("0000000000000000".to_string());

        let (mismatches, tracked, untracked) = verify_chain(&all);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].line, 2);
        assert_eq!(tracked, 2);
        assert_eq!(untracked, 0);
    }
}
