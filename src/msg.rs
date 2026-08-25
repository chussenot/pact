//! Threaded messaging over pact's own append-only store, `.pact/messages.jsonl`.
//!
//! ## Why this is not in the issue tracker any more
//!
//! Messages used to be `bd` beads (`bd create --type=message` plus
//! `--parent`/`--assignee`/`--include-infra`, with read state as one
//! `read-by-<agent>` label per reader). That made the agents' TASK TRACKER a
//! runtime dependency of pact's coordination layer, and bd 1.2 showed the bill:
//! `create --id --force` stopped upserting, four CLI tests broke with no source
//! change on pact's side, and `send` grew a duplicate-id recovery path to cope.
//!
//! The subprocess boundary insulated pact against bd's STORAGE churn — Dolt,
//! SQLite, whatever comes next — but never against its CLI-SEMANTIC churn, and
//! messages were the only pact-owned feature exposed to it. Four field runs also
//! settled what this traffic actually is: `pact watch` notices dominate it (87 and
//! 64 deliveries in two runs) while voluntary peer mail is near zero. That is
//! pact-shaped ephemeral traffic, not a backlog of issues.
//!
//! So pact owns it, under the same discipline as `.pact/events.jsonl`: one JSON
//! object per line, appended under an exclusive lock, each line chained to the one
//! before it, a torn final line counted and skipped rather than fatal, and a
//! bounded line cap. `bd` is now only ever READ, and only via its committed
//! `.beads/interactions.jsonl` export (see `crate::beads`).
//!
//! ## Two things about this store that are deliberate, and cost something
//!
//! **Messages are ephemeral, and the cap enforces it.** Past [`MAX_LINES`] the
//! oldest lines are dropped, exactly as `events.jsonl` does. For an event log that
//! is lossy history; for messages it is lost mail. That is the intended trade —
//! nothing here is a record of record, `.pact/events.jsonl` is — but it is a real
//! cost and it is why there is no importer from the bd era either.
//!
//! **Read state is local again, which reverses pact-rnc.17.** Read position lives
//! in `.pact/read/<agent>.json`, gitignored like `.pact/leases/`, because a read
//! position is per-machine by nature. pact-rnc.17 moved read state OUT of a local
//! file and INTO shared bd labels for a stated reason: with local state, a sender
//! structurally could not see whether anyone had read their message. That
//! reasoning has not been refuted — it has been narrowed. A pact fleet shares one
//! checkout, so within the case pact is actually for, `read_by` still answers
//! honestly; across machines it no longer can.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::output;
use crate::{attrs, otel};

/// `kind` for a message an agent wrote.
const MAIL: &str = "mail";

/// `kind` for a message [`crate::watch::notify_release`] generated.
///
/// It exists because watch notifications are not correspondence and must not share
/// an inbox with correspondence. In the crucible run one release of the designed
/// hot file emitted **nine** messages in nine seconds, one per watcher, and `lease
/// acquire` reported "32 unread message(s) about src/ast.rs". Sampled over that
/// window: 12 messages, 11 of them automatic release notices and exactly ONE
/// authored by an agent — a warning about six duplicate test functions that a peer
/// needed to read.
///
/// The fleet was not undisciplined; it was compliant. docs/watch.md tells agents to
/// subscribe to interfaces they depend on but do not own, and the cost of following
/// that advice is watchers × releases. So the better a fleet complies, the deeper it
/// buries the one message worth reading — the same failure the protocol block warns
/// about ("a real BLOCKER sat unread for 38 minutes"), reached structurally instead
/// of behaviourally.
///
/// This was a bd LABEL (`watch-notice`, pact-mqw.5) and is now a stored field. The
/// difference matters: a label had to be applied at creation and so was
/// forward-only, which is why there used to be a `looks_like_legacy_notice`
/// heuristic pattern-matching English in the body to recover the untagged ones. A
/// field every row carries needs no such rescue, and that heuristic is gone.
const NOTICE: &str = "watch-notice";

/// The join between the subject `pact watch` writes and the path the inbox groups
/// notices by.
///
/// A const because both sides live in different modules and the format is the only
/// thing tying them together — [`crate::watch::notify_release`] builds the subject
/// with it and [`split_notices`] parses the path back out of it. A notice whose
/// subject does not contain it still counts, it just groups under its whole
/// subject, so drift degrades the grouping instead of losing messages.
pub const NOTICE_SUBJECT_MARKER: &str = " changed — released by ";

/// The same join, for a notice that carries no diff because there was never any
/// content to diff (pact-bsf).
///
/// A reserved key like `.pact/internal/merge-to-master` is a name, not a file, so
/// "changed" would be a lie: nothing changed, the lock was let go. Two markers
/// rather than one generalised string because [`notice_path`] must find whichever
/// built the subject, and because the subject is read by an agent, not only parsed.
pub const NOTICE_FREED_MARKER: &str = " freed — released by ";

/// The path a notice subject names, whichever marker built it.
///
/// `split_once` rather than `split(..).next()`: the latter returns the whole string
/// when the marker is absent, so trying two markers in sequence with it would have
/// the first always "succeed" and the second never run.
fn notice_path(subject: &str) -> &str {
    for marker in [NOTICE_SUBJECT_MARKER, NOTICE_FREED_MARKER] {
        if let Some((path, _)) = subject.split_once(marker) {
            return path;
        }
    }
    // Drift degrades the grouping instead of losing the message — see above.
    subject
}

/// Past this many lines, [`append`] compacts down to [`KEEP_LINES`].
///
/// The same pair as `events.jsonl`, on purpose: one discipline, one set of numbers
/// to reason about. Unlike the event log, what gets dropped here is mail — see the
/// module docs on why that is accepted rather than merely tolerated.
const MAX_LINES: usize = 5000;
const KEEP_LINES: usize = 4000;

/// One stored message. The wire format of `.pact/messages.jsonl`.
///
/// Field names follow `events::Event` rather than inventing a second vocabulary
/// (`at`, not `ts`), so anyone who can read one file can read the other. The
/// PUBLIC shape agents see is [`Message`], which keeps `created_at` and the rest of
/// the keys `--json` consumers were already pinned to.
///
/// Every optional field is `skip_serializing_if`, so a line stays as narrow as what
/// it actually says — the same rule every field added to `Event` has followed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Content hash, from [`message_id`]. Not a counter and not random: see that
    /// function for the three production incidents that bought this property.
    pub id: String,
    pub at: String,
    /// The sending agent's `PACT_AGENT`.
    pub from: String,
    /// Every recipient of this send, first-seen order.
    ///
    /// ONE row for N recipients, where bd needed N beads because a bead has exactly
    /// one assignee. That deletes the whole parent-child fan-out that existed only
    /// to make N beads read as one thread.
    pub to: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub body: String,
    /// The thread this belongs to; a root message's own id.
    pub thread: String,
    /// [`MAIL`] or [`NOTICE`].
    pub kind: String,
    /// The message this replies to, when it is a reply to a specific one rather
    /// than to the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// Paths this message is ABOUT, from `--to-owner-of`.
    ///
    /// RAW paths. As bd labels these had to be encoded — `/` to `__`, and every
    /// byte outside `[A-Za-z0-9_:-]` to `-`, because br rejected a `.` outright and
    /// so every real filename silently failed to tag. That encoding was lossy, not
    /// reversible, and had to be applied identically at write and query time with a
    /// legacy-encoding fallback for rows written before it was narrowed. A JSON
    /// string needs none of it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub about: Vec<String>,
    /// This line's link to the one before it. `None` on a line written before chain
    /// tracking, which is history this feature knows nothing about rather than a gap
    /// to see through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_hash: Option<String>,
    /// The SENDER's harness, when detectable — `"claude-code"`, or whatever
    /// `PACT_HARNESS` declares (pact-c3y).
    ///
    /// The sender's, never the reader's: this row is written once, by the agent
    /// sending it, and every later reader sees the same line. `read_by` is where
    /// readers are recorded, and it is a separate mechanism for exactly that
    /// reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The sender's model — declared by the launcher; verified, if ever, by
    /// joining session records (see recount). See [`crate::harness::model`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The sender's harness session id, when the harness exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session: Option<String>,
    /// The sender's harness subagent id — the key recount joins a transcript on.
    /// Sparse by measurement, see [`crate::events::Event::harness_subagent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_subagent: Option<String>,
    /// `kind: "handoff"` only — the bead whose findings these are (pact-e7d).
    ///
    /// A field rather than a substring of the subject, and the reason is the one
    /// this repository keeps relearning: `pact audit`'s coverage line has to ask
    /// "which beads with dependents left findings", and answering that by
    /// regexing `handoff from <id>` out of prose would make the answer depend on
    /// a sentence anybody could reword. The thread says who it is FOR; this says
    /// who it is FROM.
    ///
    /// Absent on every other kind, and on handoffs written before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_from: Option<String>,
}

/// One message as every `--json` consumer already knows it.
///
/// Deliberately NOT the stored shape. A [`Record`] with `to: [a, b]` fans out to one
/// `Message` per recipient, so `to` stays a single agent name and `msg send`'s array
/// still has one entry per recipient. The alternative — one object with the
/// recipients joined into a string — would break `jq -r .to` silently, which is the
/// worst of the available failures.
#[derive(Debug, Serialize)]
pub struct Message {
    /// The stored row's content hash. Shared by every fanned-out copy, where the bd
    /// era gave each recipient a distinct bead id: any recipient's id now resolves
    /// to the same thread, which is what `--thread` wanted all along.
    pub id: String,
    pub thread: String,
    pub from: String,
    pub to: String,
    pub subject: Option<String>,
    pub body: String,
    pub created_at: String,
    /// Read by the querying agent (for [`all_messages`], which has no querying
    /// agent, read by this copy's own recipient).
    pub read: bool,
    /// Every agent that has read this message, from the read cursors under
    /// `.pact/read/`.
    pub read_by: Vec<String>,
    /// Machine-generated by `pact watch`, from [`Record::kind`].
    pub notice: bool,
}

/// Everything about a message except who it goes to.
///
/// A struct rather than four more parameters: `send` was already at the limit, and
/// "the message" is a real thing with parts, not an argument list.
pub struct Draft<'a> {
    /// Reply within an existing thread; `None` starts a new one.
    pub thread: Option<&'a str>,
    pub subject: Option<&'a str>,
    pub body: &'a str,
    /// Paths this message is ABOUT, from `--to-owner-of`. Recorded so delivery can
    /// follow the file after the agent it resolved to has exited.
    pub about: &'a [String],
    /// Machine-generated by `pact watch`, not written by an agent. Set only by
    /// [`crate::watch::notify_release`]; every CLI path leaves it false, because
    /// there is no flag for "pretend I am a robot" and there should not be.
    pub notice: bool,
}

// ------------------------------------------------------------------ the store

fn messages_file_path(repo_root: &Path) -> PathBuf {
    crate::repo::pact_dir_path(repo_root).join("messages.jsonl")
}

/// The store's path, creating `.pact/` if it is not there yet.
///
/// Split from [`messages_file_path`] because "a question must not mutate"
/// (CLAUDE.md): every read path resolves the path without creating anything, and
/// only [`append`] uses this one.
fn messages_file(repo_root: &Path) -> Result<PathBuf> {
    Ok(crate::repo::pact_dir(repo_root)?.join("messages.jsonl"))
}

/// The store's half of the shared append-only discipline (`events::jsonl`).
///
/// `chain_hash` is spelled as a plain field here and reached through the trait,
/// which is what lets `events.jsonl`, `.pact/watches.jsonl` and this file share one
/// append implementation instead of keeping three copies of the same locking,
/// chaining and trimming logic in step by hand.
impl crate::events::jsonl::Chained for Record {
    fn chain_hash(&self) -> Option<&str> {
        self.chain_hash.as_deref()
    }

    fn set_chain_hash(&mut self, hash: Option<String>) {
        self.chain_hash = hash;
    }
}

/// Every stored message, oldest first, plus how many lines were unparseable.
///
/// Tolerant by contract, not by accident: a torn final line — an append that wrote
/// part of a line and then failed — is counted and skipped, exactly as
/// `events::all` and `watch::records` treat theirs. Duplicate ids collapse to the
/// first occurrence, which is what makes a re-sent message a no-op instead of a
/// second delivery.
pub fn records(repo_root: &Path) -> Result<(Vec<Record>, usize)> {
    let (rows, skipped) = crate::events::jsonl::read::<Record>(&messages_file_path(repo_root))?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (_, r) in rows {
        // First occurrence wins. A duplicate id is a replayed send, and the earlier
        // line is the one whose `at` reflects when the message really was first
        // delivered — which is what makes a retry a no-op rather than a second,
        // later-stamped delivery.
        if seen.insert(r.id.clone()) {
            out.push(r);
        }
    }
    out.sort_by_key(|r| parse_ts(&r.at));
    Ok((out, skipped))
}

/// Append one message, chained to the line before it, under the shared lock.
///
/// Fallible, unlike `events::append`, and that difference is intentional: an event is
/// a side-record of something that already happened, so losing one must never fail
/// the operation it describes, whereas a message that was not written was not sent
/// and the sender has to be told.
fn append(repo_root: &Path, record: &Record) -> Result<()> {
    let path = messages_file(repo_root)?;
    crate::events::jsonl::append(&path, record, Some((MAX_LINES, KEEP_LINES)))
}

// ------------------------------------------------------------- read cursors

/// Where one agent's read positions live.
///
/// Agent names are `[a-z0-9][a-z0-9-]{1,31}` (`identity::validate`), so they are
/// filename-safe by construction — but a RECIPIENT name arrives from `--to` and is
/// not put through that gate, so anything outside the safe set is collapsed here
/// rather than trusted into a path. A trust boundary is not a place to save three
/// lines.
fn cursor_path(repo_root: &Path, agent: &str) -> PathBuf {
    let safe: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    crate::repo::pact_dir_path(repo_root)
        .join("read")
        .join(format!("{safe}.json"))
}

/// One agent's read positions: message id -> when they read it.
///
/// A map, not a high-water mark. A mark cannot say "read 5 but not 3", which the
/// `read-by-` labels could, and an inbox that silently marks skipped mail read is
/// worse than one that keeps a slightly larger file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    #[serde(default)]
    read: BTreeMap<String, String>,
}

/// Best-effort: an unreadable or malformed cursor reads as "has read nothing".
///
/// Never an error. This is local, regenerable state, and the cost of getting it
/// wrong in that direction is showing a message as unread twice — against failing a
/// command that had nothing to do with it.
fn cursor(repo_root: &Path, agent: &str) -> Cursor {
    std::fs::read_to_string(cursor_path(repo_root, agent))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Record `ids` as read by `agent`, at `now`. Idempotent.
fn remember_read(repo_root: &Path, agent: &str, ids: &[String], now: &str) -> Result<()> {
    let mut cur = cursor(repo_root, agent);
    let mut changed = false;
    for id in ids {
        changed |= cur.read.insert(id.clone(), now.to_string()).is_none();
    }
    if !changed {
        return Ok(());
    }
    let path = cursor_path(repo_root, agent);
    let dir = path.parent().context("read cursor has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Written via a uniquely-named temp and renamed, so a reader never sees a
    // half-written cursor and two agents writing their own cursors cannot collide
    // on the temp name. Same reasoning as the event log's append.
    let tmp = dir.join(crate::events::unique_temp_name("read"));
    std::fs::write(&tmp, serde_json::to_string(&cur)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Which agents have read each message, across every cursor in the repo.
///
/// One directory scan for the whole listing rather than one per message: an inbox of
/// 200 notices must not become 200 file reads.
fn readers(repo_root: &Path) -> BTreeMap<String, Vec<String>> {
    let dir = crate::repo::pact_dir_path(repo_root).join("read");
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(agent) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(cur) = serde_json::from_str::<Cursor>(&text) else {
            continue;
        };
        for id in cur.read.keys() {
            out.entry(id.clone()).or_default().push(agent.to_string());
        }
    }
    for agents in out.values_mut() {
        agents.sort();
        agents.dedup();
    }
    out
}

// --------------------------------------------------------------- ids and fan-out

/// Non-cryptographic FNV-1a 64-bit mix of the parts, with a separator between them.
fn fnv1a64(parts: &[&str]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // A separator between parts, so ("ab", "c") and ("a", "bc") — which would
        // otherwise concatenate to the same byte stream — hash differently.
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// A message's id: a hash of its own content, so a retried send lands on the SAME
/// message instead of a second, near-identical one.
///
/// This is not a new idea for this store — it is `idempotency_key`, kept. It was
/// built after three production incidents in which a send's outcome was ambiguous
/// and the retry minted a duplicate: a process killed after bd committed but before
/// returning, a hung subprocess, and a harness that dropped stdout (pact-m7j.6.4).
/// `sent()`'s own doc tells a sender who cannot confirm a send to re-send it, so
/// without this the documented policy is what mints the duplicate.
///
/// Two things changed by moving off bd, both improvements:
///
/// - It now covers REPLIES too. On bd this key could only ride a root message,
///   because a `create` could not carry `--id` alongside `--parent`, so every reply
///   was unprotected. A row in a file has no such restriction.
/// - The recovery is gone with the mechanism that needed it. bd ≤1.1 upserted on
///   `--id`/`--force`; bd 1.2 refuses with "already exists" and exits 1, so `send`
///   had to read that refusal as proof of delivery (pact-0re). Here a duplicate is
///   simply a line [`records`] collapses.
///
/// `at` is deliberately NOT an input, which is the whole point — and the accepted
/// trade-off, unchanged from the bd era: two SEPARATE, genuinely identical sends
/// (same sender, same recipients, same thread, byte-identical subject and body)
/// collapse into one message rather than two. For the mostly-unique text real
/// messages carry that is a good bargain against a duplicate that already happened
/// in production. It is not a content-addressed store's collision resistance and
/// was never asked to be.
fn message_id(
    from: &str,
    to: &[String],
    thread: Option<&str>,
    subject: &str,
    body: &str,
) -> String {
    let recipients = to.join(",");
    let hash = fnv1a64(&[from, &recipients, thread.unwrap_or(""), subject, body]);
    format!("pact-msg-{hash:016x}")
}

/// `kind` for a handoff: findings left for whoever picks up a dependent bead
/// (pact-e7d).
///
/// A third kind beside [`MAIL`] and [`NOTICE`], additive by the same rule they
/// were: a pact that predates it reads the row, does not recognise the kind, and
/// treats it as ordinary correspondence. Nothing breaks and nothing is hidden.
pub const HANDOFF: &str = "handoff";

/// The thread a bead's inheritance lives on.
///
/// **The address is the WORK, not a person**, and that is the whole reason this
/// feature can exist. A handoff is written when a bead closes, for whoever picks
/// up what depended on it — and that agent frequently has not been spawned yet,
/// so there is no name to send to. A path already outlives its holder in
/// `--to-owner-of`; a bead outlives everyone.
pub fn bead_thread(bead: &str) -> String {
    format!("bead:{bead}")
}

/// How much of a handoff is kept.
///
/// The same bound `watch`'s release diffs carry, for the same measured reason:
/// past a point a wall of text is not a message but a pointer to one, and the
/// reader stops reading. Handoffs are written by an agent that has just finished
/// something and has everything in its context, which is exactly the condition
/// that produces a wall.
const MAX_HANDOFF_LINES: usize = 200;

/// Post `body` to a bead's thread, addressed to nobody.
///
/// `to` is empty, and that is not a degenerate case — it is the shape. Putting the
/// dependent's agent there is impossible (it may not exist), and putting the bead
/// id there would be worse: `to` is an agent field, and `agents::observe` would
/// enrol every bead id as an identity in the roster and in recipient validation.
///
/// The consequence, stated because it decides how a handoff is read: [`fan_out`]
/// produces one [`Message`] per recipient, so a record with none fans out to
/// nothing and is invisible to `inbox` and to [`all_messages`]. A handoff is
/// reached through its THREAD — `pact msg thread bead:<id>` — and nowhere else.
pub fn post_to_thread(
    repo_root: &Path,
    agent: &str,
    thread: &str,
    from_bead: &str,
    subject: &str,
    body: &str,
) -> Result<Record> {
    let body = cap_handoff(body);
    let id = message_id(agent, &[], Some(thread), subject, &body);
    let record = Record {
        thread: thread.to_string(),
        id,
        at: Utc::now().to_rfc3339(),
        from: agent.to_string(),
        to: Vec::new(),
        subject: Some(subject.to_string()),
        body,
        kind: HANDOFF.to_string(),
        in_reply_to: None,
        about: Vec::new(),
        chain_hash: None,
        handoff_from: Some(from_bead.to_string()),
        ..attribution_of()
    };
    append(repo_root, &record)?;
    Ok(record)
}

/// Every stored record, oldest first.
///
/// Records rather than [`Message`]s, for the reason [`thread_records`] gives: a
/// handoff has no recipients and fans out to nothing.
pub fn all_records(repo_root: &Path) -> Result<Vec<Record>> {
    Ok(records(repo_root)?.0)
}

/// Every record on `thread`, oldest first.
///
/// Reads RECORDS rather than [`Message`]s on purpose: a handoff has no recipients,
/// so the fan-out that produces `Message` drops it entirely. This is the only
/// route to one.
pub fn thread_records(repo_root: &Path, thread: &str) -> Result<Vec<Record>> {
    let (records, _) = records(repo_root)?;
    Ok(records.into_iter().filter(|r| r.thread == thread).collect())
}

/// Trim a handoff to [`MAX_HANDOFF_LINES`], saying where the rest went.
///
/// Names the cap in the text rather than trailing off, so a reader can tell a
/// truncated handoff from a terse one — the same distinction `watch`'s diff
/// truncation draws, and for the same reason: silence and brevity look identical
/// at the bottom of a message.
fn cap_handoff(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= MAX_HANDOFF_LINES {
        return body.to_string();
    }
    let kept = lines[..MAX_HANDOFF_LINES].join("\n");
    format!(
        "{kept}\n\n[handoff truncated at {MAX_HANDOFF_LINES} lines of {}. A handoff is a \
         summary for somebody who has not read your work; if the rest matters, put it in the \
         repository and name the path here.]",
        lines.len()
    )
}

/// The sender's attribution, as every stored record carries it.
fn attribution_of() -> Record {
    let who = crate::harness::Attribution::resolve();
    Record {
        id: String::new(),
        at: String::new(),
        from: String::new(),
        to: Vec::new(),
        subject: None,
        body: String::new(),
        thread: String::new(),
        kind: String::new(),
        in_reply_to: None,
        about: Vec::new(),
        chain_hash: None,
        handoff_from: None,
        harness: who.harness,
        model: who.model,
        harness_session: who.session,
        harness_subagent: who.subagent,
    }
}

/// One [`Record`] as one [`Message`] per recipient, in stored recipient order.
///
/// `viewer` is the agent asking; `None` means "resolve `read` against each copy's
/// own recipient", which is what a recipient-agnostic listing wants.
fn fan_out(
    record: &Record,
    viewer: Option<&str>,
    readers: &BTreeMap<String, Vec<String>>,
) -> Vec<Message> {
    let read_by = readers.get(&record.id).cloned().unwrap_or_default();
    record
        .to
        .iter()
        .map(|recipient| Message {
            id: record.id.clone(),
            thread: record.thread.clone(),
            from: record.from.clone(),
            to: recipient.clone(),
            subject: record.subject.clone(),
            body: record.body.clone(),
            created_at: record.at.clone(),
            read: read_by.iter().any(|a| a == viewer.unwrap_or(recipient)),
            read_by: read_by.clone(),
            notice: record.kind == NOTICE,
        })
        .collect()
}

/// Every stored message fanned out, oldest first.
fn all(repo_root: &Path, viewer: Option<&str>) -> Result<Vec<Message>> {
    let (records, _) = records(repo_root)?;
    let readers = readers(repo_root);
    let mut out: Vec<Message> = records
        .iter()
        .flat_map(|r| fan_out(r, viewer, &readers))
        .collect();
    out.sort_by(oldest_first);
    Ok(out)
}

// ------------------------------------------------------------------ the API

/// Send one message to one or more recipients, as ONE thread.
///
/// One row, however many recipients. The bd era needed N beads for N recipients
/// (a bead has exactly one assignee) and then made them read as one conversation by
/// parenting recipients 2..N on the thread root — children of the root rather than
/// of each other, because `read_thread` returned direct children only and a
/// grandchild was invisible in the thread a reader actually opened. None of that
/// machinery survives: a row carries its whole recipient list and its own thread id.
///
/// Atomic, which the bd version could not be. There is no partial fan-out to report
/// any more, so [`Draft`]'s send either happened or errored — which is why the
/// `SendFailure`/`already_sent` JSON error shape is gone.
pub fn send(
    repo_root: &Path,
    agent: &str,
    to: &[String],
    draft: Draft<'_>,
) -> Result<Vec<Message>> {
    let Draft {
        thread,
        subject,
        body,
        about,
        notice,
    } = draft;
    if to.is_empty() {
        anyhow::bail!("no recipients — `msg send` needs at least one --to");
    }
    // One recipient named twice is one recipient. Without this, `--to a --to a`
    // delivered the same message to the same inbox twice — reproducible, and
    // silent, because pact has no uniqueness constraint to trip over.
    //
    // The realistic caller is not a human typing the flag twice: the protocol block
    // tells agents to repeat `--to` for a multi-recipient decision, so a list built
    // from `pact agents --json` or an orchestrator template can repeat a name. And
    // `pact msg sent` exists precisely because a previous fleet produced duplicate
    // messages, so a command that manufactures them works against the tool's own
    // advice.
    //
    // First-seen order is preserved: the recipient list a reader sees must not be
    // reordered because a later duplicate was dropped.
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<String> = to.iter().filter(|r| seen.insert(*r)).cloned().collect();
    let dropped = to.len() - deduped.len();
    if dropped > 0 {
        // Said out loud, not swallowed: a caller that repeated a name probably built
        // the list wrongly and should find out now.
        output::warn(&format!(
            "note: {dropped} duplicate recipient(s) collapsed — sending one message per distinct agent"
        ));
    }
    let to = deduped;
    let title = subject
        .map(str::to_string)
        .unwrap_or_else(|| default_subject(body));

    // Resolved to the thread ROOT rather than used verbatim. An agent legitimately
    // holds a non-root member id — `msg read` prints the ids in a thread — and a
    // reply must join the thread rather than start a sub-thread nobody reading the
    // conversation would see. Prefix-resolved, so the id an agent copied off a
    // listing works whether or not they took all of it.
    let parent = match thread {
        Some(t) => Some(resolve_id(repo_root, t)?),
        None => None,
    };
    let thread_id = parent.as_ref().map(|r| r.thread.clone());
    // `about` arrives ALREADY normalized, and must not be normalized again here.
    // `run_msg` canonicalizes `--to-owner-of` before it does anything with it
    // (pact-m7j.8.6) because it needs that spelling for its own `events::owner_of`
    // lookup too; `normalize_path` resolves against the process CWD, so applying it
    // to a path that is already repo-relative mangles it whenever the caller is not
    // sitting at the repo root. `about_path` normalizes its QUERY, which is the other
    // half of the same contract.
    // Deliberately NOT an input to `message_id` (pact-c3y). The id is what makes a
    // retried send land on the same message, and a retry is very often a DIFFERENT
    // process — a resumed subagent, a re-spawned agent, an orchestrator finishing
    // what a killed one started. Hashing the sender's harness session into the id
    // would mint precisely the duplicate that key exists to prevent, in exactly the
    // ambiguous-outcome case it was built for.
    let who = crate::harness::Attribution::resolve();
    let id = message_id(agent, &to, thread_id.as_deref(), &title, body);
    let record = Record {
        thread: thread_id.clone().unwrap_or_else(|| id.clone()),
        id,
        at: Utc::now().to_rfc3339(),
        from: agent.to_string(),
        to,
        subject: Some(title),
        body: body.to_string(),
        kind: if notice { NOTICE } else { MAIL }.to_string(),
        in_reply_to: parent.map(|r| r.id),
        about: about.to_vec(),
        chain_hash: None,
        // Ordinary correspondence inherits nothing: this is a handoff-only field.
        handoff_from: None,
        harness: who.harness,
        model: who.model,
        harness_session: who.session,
        harness_subagent: who.subagent,
    };
    append(repo_root, &record)?;
    // Read back the PERSISTED row rather than reporting the one just built. On a
    // replay the store keeps the FIRST delivery's line, so the freshly-built record's
    // `at` is this call's wall clock and not when the message was really sent — and
    // `--json` reporting the retry's own timestamp is exactly the confusion the
    // deterministic id exists to prevent (pact-m7j.6.7 made the same fix against bd's
    // `create` echo). Falls back to the local record if the read cannot find it,
    // because a send that landed must not report failure.
    let stored = records(repo_root)
        .ok()
        .and_then(|(rows, _)| rows.into_iter().find(|r| r.id == record.id))
        .unwrap_or(record);
    let record = stored;

    let addressing = addressing_mode();
    let is_reply = thread.is_some();
    for _ in &record.to {
        otel::count(
            "pact.msg.sent",
            1,
            &attrs![
                "pact.msg.addressing" => addressing,
                "pact.msg.reply" => is_reply,
            ],
        );
    }
    Ok(fan_out(&record, None, &readers(repo_root)))
}

/// Messages tagged as being about `path`, for any recipient, oldest first.
///
/// This is what makes `--to-owner-of` a delivery mechanism rather than an
/// address-book lookup. It deliberately ignores who a message was addressed to: the
/// point is that a message about `src/otel.rs` reaches whoever picks up
/// `src/otel.rs`, even — especially — when the agent it was addressed to has exited.
/// Reading it is the recipient's job; noticing it is the file's.
///
/// `--to-owner-of` addressed a file and then resolved it to an agent, and delivery
/// stopped there. Measured over one fleet run: 30 of 44 agent-to-agent messages went
/// to agents who had already exited, and none of those were ever read, while every
/// message to a live agent was read. Addressing was never the failure —
/// deliverability was. Every one of the 30 was about a file, sent to the agent who
/// had just released it (pact-4tj).
pub fn about_path(repo_root: &Path, path: &str) -> Result<Vec<Message>> {
    let relative = crate::lease::normalize_path(repo_root, path);
    let (records, _) = records(repo_root)?;
    let readers = readers(repo_root);
    let mut out: Vec<Message> = records
        .iter()
        .filter(|r| r.about.contains(&relative))
        .flat_map(|r| fan_out(r, None, &readers))
        .collect();
    out.sort_by(oldest_first);
    Ok(out)
}

/// Messages addressed to `agent`, oldest first.
pub fn inbox(repo_root: &Path, agent: &str, unread_only: bool) -> Result<Vec<Message>> {
    let mut messages = all(repo_root, Some(agent))?;
    messages.retain(|m| m.to == agent);

    // Before the filter, so `--unread-only` and a plain listing report the same queue
    // depth. This is the observation pact-aw7.4 exists for: nobody can see a mailbox
    // rotting from inside the process that is not reading it.
    record_unread(&messages, Utc::now());

    if unread_only {
        messages.retain(|m| !m.read);
    }
    Ok(messages)
}

/// Messages `agent` sent, newest first, and whether they were read.
pub fn sent(repo_root: &Path, agent: &str) -> Result<Vec<Message>> {
    let mut messages = all(repo_root, None)?;
    messages.retain(|m| m.from == agent);
    messages.reverse(); // `all` is oldest-first
    Ok(messages)
}

/// Every message in the repo, regardless of recipient, oldest first.
///
/// There is no querying agent here, so `read` is resolved against each copy's own
/// recipient.
pub fn all_messages(repo_root: &Path) -> Result<Vec<Message>> {
    all(repo_root, None)
}

/// Messages whose own recipient never marked them read, oldest first.
///
/// Leans on [`all_messages`] resolving `read` against each copy's own recipient, so
/// this is exactly "nobody the message was addressed to has acknowledged it", not "I
/// have not read it".
///
/// Why this is worth a check of its own: `pact msg sent` already shows a sender
/// whether their message landed, but only that sender, only for their own messages,
/// and only if they think to look. A fleet field run (megablast) ended with one
/// message unacknowledged — an agent warning the owner of a file it did not own that
/// a constant it had just changed would panic at runtime if `MAX_QUADS` was not
/// updated with it. The warning was acted on, correctly, but never marked read, so
/// `pact msg sent` reported it undelivered permanently and nothing anywhere said so.
pub fn unacknowledged(repo_root: &Path) -> Result<Vec<Message>> {
    let mut messages = all_messages(repo_root)?;
    messages.retain(|m| !m.read);
    Ok(messages)
}

/// Resolve a possibly-abbreviated id to exactly one stored message id.
///
/// Ids are content hashes now, not `pact-abc` bead ids, so they are longer than
/// anything an agent wants to retype — which makes prefix addressing a requirement
/// rather than a convenience. An ambiguous prefix is an ERROR listing the
/// candidates, never a guess: picking one would silently reply into the wrong
/// thread.
fn resolve_id(repo_root: &Path, id: &str) -> Result<Record> {
    let (records, _) = records(repo_root)?;
    if let Some(exact) = records.iter().find(|r| r.id == id) {
        return Ok(exact.clone());
    }
    let mut hits: Vec<&Record> = records.iter().filter(|r| r.id.starts_with(id)).collect();
    match hits.len() {
        1 => Ok(hits.remove(0).clone()),
        0 => anyhow::bail!("no message matching {id}"),
        n => anyhow::bail!(
            "{id} matches {n} messages — use more of the id: {}",
            hits.iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The thread id `id` belongs to.
///
/// Every member of a thread must resolve to the SAME thread id, on every surface
/// (pact-rnc.4): `msg inbox` reports the thread, so `msg read` reporting the queried
/// id meant two pact commands disagreeing, and the id `msg read` prints is a
/// recipient's only source. A stored `thread` field makes that one field read instead
/// of a parent walk with a depth cap.
fn resolve_thread(repo_root: &Path, id: &str) -> Result<String> {
    Ok(resolve_id(repo_root, id)?.thread)
}

/// Every message in the thread `id` belongs to, oldest first, WITHOUT marking
/// anything read.
///
/// The non-marking twin of [`read_thread`], for the read-only MCP server (`pact mcp
/// serve`), where answering "what is in this thread" must not change delivery state.
/// `read_thread` writes a read cursor, and that cursor is what a *sender* checks with
/// `msg sent` to decide whether a decision landed — so an observer who marked threads
/// read while looking at them would silently tell every sender their message had been
/// received by an agent that never saw it.
///
/// `viewer` only decides whose `read` flag is reported; passing `None` reports the
/// recipient's own.
///
/// Gated on the feature that uses it, or the default build warns it dead. That is not
/// a formality — `mark_read_by_id` shipped ungated, went red in CI on the default
/// build only, and was missed locally because `mise run check` was running with
/// `--features ui` and nothing else.
#[cfg(feature = "mcp")]
pub fn peek_thread(repo_root: &Path, viewer: Option<&str>, id: &str) -> Result<Vec<Message>> {
    let thread = resolve_thread(repo_root, id)?;
    let (records, _) = records(repo_root)?;
    let readers = readers(repo_root);
    Ok(records
        .iter()
        .filter(|r| r.thread == thread)
        .flat_map(|r| fan_out(r, viewer, &readers))
        .collect())
}

/// Every message in the thread `id` belongs to, oldest first. Marks them read for
/// `agent`.
pub fn read_thread(repo_root: &Path, agent: &str, id: &str) -> Result<Vec<Message>> {
    let thread = resolve_thread(repo_root, id)?;
    let (records, _) = records(repo_root)?;
    let shown: Vec<&Record> = records.iter().filter(|r| r.thread == thread).collect();
    let ids: Vec<String> = shown.iter().map(|r| r.id.clone()).collect();

    let now = Utc::now();
    // Bookkeeping must not destroy the thread the caller came for: if the cursor
    // write fails, warn and show the messages anyway. They stay unread, so the next
    // read retries. Same reasoning as pact-rnc.26 — never fail work that already
    // succeeded.
    //
    // The bd version verified the write by re-fetching every bead and confirming the
    // label had landed, because a subprocess exiting 0 is not proof that the specific
    // change it was asked to make is the one that landed (the same argument
    // `lease::verify_own_lease` makes). A local file write that returns `Ok` IS that
    // proof, so the verification pass goes with the subprocess it was guarding.
    let marked = match remember_read(repo_root, agent, &ids, &now.to_rfc3339()) {
        Ok(()) => true,
        Err(e) => {
            output::warn(&format!(
                "warning: could not mark thread {thread} read: {e:#}"
            ));
            false
        }
    };

    let readers = readers(repo_root);
    Ok(shown
        .iter()
        .flat_map(|r| fan_out(r, Some(agent), &readers))
        .map(|mut m| {
            // Just recorded, so a snapshot taken before the write would not show it.
            if marked && !m.read {
                m.read = true;
                m.read_by.push(agent.to_string());
                m.read_by.sort();
                m.read_by.dedup();
                // This branch *is* "first read by this agent" — re-reading a thread
                // takes the other one — so it is the event to count, and the
                // message's age here is how long the sender waited.
                otel::count("pact.msg.read", 1, &attrs![]);
                if let Some(ms) = age_ms(&m.created_at, now) {
                    otel::record_ms("pact.msg.read_latency", ms, &attrs![]);
                }
            }
            m
        })
        .collect())
}

/// Mark one message read by id, for a caller that has the id but not the thread.
///
/// `pact ui` needs this: the dashboard is the human's inbox, and until it could
/// record a read the sender's `pact msg sent` said "unread" forever (pact-4tj).
/// `pact ui` is the only caller, so this is dead code in a default build — gated
/// rather than `allow`ed, because an `allow` would also hide the day it stops being
/// called at all.
#[cfg(feature = "ui")]
pub fn mark_read_by_id(repo_root: &Path, agent: &str, id: &str) -> Result<()> {
    let record = resolve_id(repo_root, id)?;
    remember_read(repo_root, agent, &[record.id], &Utc::now().to_rfc3339())
}

/// Which `pact watch` notices an inbox listing should include.
///
/// `Authored` is the default, and that is the whole point of the type: a notification
/// is a side effect of a peer doing its job, and an inbox is where an agent looks for
/// things addressed to it *by somebody*. See [`NOTICE`] for the run that made the
/// distinction necessary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatchView {
    /// Authored messages only, with notices reported as a trailing count.
    #[default]
    Authored,
    /// Everything, notices included.
    Include,
    /// Notices only — "what changed under me while I was working".
    Only,
}

/// Consecutive unread notices for one path, collapsed into one entry.
///
/// The count is what a reader needs; the individual bodies are not. Nine diffs of one
/// file, delivered nine seconds apart, answer one question — "what did this file
/// become" — and only the last of them answers it. The earlier eight are superseded
/// by the time anyone reads them, which is exactly why they are summarised rather
/// than listed.
#[derive(Debug, Serialize)]
pub struct NoticeGroup {
    /// The path, parsed out of the subject via [`NOTICE_SUBJECT_MARKER`], or the
    /// whole subject when that marker is absent.
    pub path: String,
    /// How many notices this group stands for.
    pub count: usize,
    /// The most recent releaser — the one whose diff is still current.
    pub latest_from: String,
    /// The most recent notice's id, so `pact msg read <id>` reaches the diff that has
    /// not been superseded.
    pub latest_id: String,
    pub latest_at: String,
    /// How many of the group are unread by the viewer.
    pub unread: usize,
}

/// Split a listing into authored messages and per-path notice groups.
///
/// Pure, so the grouping is unit-testable without a store, and shared by every
/// renderer rather than reimplemented per surface. Input order is oldest-first, so
/// "latest" is the last one seen per path and the returned groups keep first-notice
/// order — a stable listing across runs.
pub fn split_notices(messages: &[Message]) -> (Vec<&Message>, Vec<NoticeGroup>) {
    let mut authored = Vec::new();
    let mut groups: Vec<NoticeGroup> = Vec::new();
    for m in messages {
        if !m.notice {
            authored.push(m);
            continue;
        }
        let path = m
            .subject
            .as_deref()
            .map(notice_path)
            .unwrap_or_default()
            .to_string();
        let unread = usize::from(!m.read);
        match groups.iter_mut().find(|g| g.path == path) {
            Some(g) => {
                g.count += 1;
                g.unread += unread;
                g.latest_from = m.from.clone();
                g.latest_id = m.id.clone();
                g.latest_at = m.created_at.clone();
            }
            None => groups.push(NoticeGroup {
                path,
                count: 1,
                latest_from: m.from.clone(),
                latest_id: m.id.clone(),
                latest_at: m.created_at.clone(),
                unread,
            }),
        }
    }
    (authored, groups)
}

// --------------------------------------------------------------------- helpers

/// pact-rnc.20: compare parsed instants, never the raw strings. Two writers reached
/// these lists in the bd era — bd's `Z` and pact's own chrono `+00:00` — and `'+'`
/// (0x2B) sorts before `'Z'` (0x5A), so a string compare calls an older `Z` stamp
/// newer than a `+00:00` one. Only pact writes them now, but a store can still hold
/// lines from both eras, and unparsable must keep sorting oldest (None < Some) rather
/// than blowing up.
fn oldest_first(a: &Message, b: &Message) -> Ordering {
    parse_ts(&a.created_at).cmp(&parse_ts(&b.created_at))
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn default_subject(body: &str) -> String {
    let first_line = body.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return "(no subject)".to_string();
    }
    if first_line.chars().count() > 60 {
        let truncated: String = first_line.chars().take(57).collect();
        format!("{truncated}...")
    } else {
        first_line.to_string()
    }
}

// ---------------------------------------------------------------------------
// Telemetry (pact-aw7.4)
//
// 51 of 59 messages in one fleet run were never read, and it took a human
// reading `pact log` afterwards to find that out. The one that mattered —
// "BLOCKER: pact init deletes the protocol" — sat unread for 38 minutes in the
// mailbox of an agent that had already exited. None of that is visible from
// inside any single process, so it has to be counted and shipped.
//
// Counts and ages only, never a subject and never a body: a message body is
// user free text and is not ours to send off this machine.
// ---------------------------------------------------------------------------

/// Age buckets for unread mail, in seconds, as an *attribute* rather than a
/// histogram. The shared explicit bounds in `otel.rs` stop at 10 s, which is
/// the right scale for a `bd` subprocess and cannot express "unread for 38
/// minutes" — every real value would land in the overflow bucket. Five
/// source-controlled labels keep the distribution and keep the dimension
/// bounded; a message id or a path here would be unbounded.
const UNREAD_AGE_BUCKETS: [(&str, i64); 5] = [
    ("lt_1m", 60),
    ("1m_5m", 300),
    ("5m_15m", 900),
    ("15m_1h", 3600),
    ("gt_1h", i64::MAX),
];

/// Which flag actually addressed this send. `--to-owner-of` exists because a
/// path outlives the process that held it (pact-o38), and whether the fleet
/// adopted it is a number, not a feeling.
///
/// Taken from argv rather than plumbed down from `main`, because by the time
/// `send` is reached the two are indistinguishable: `--to-owner-of <path>` has
/// already been resolved to an agent name and pushed onto the same `to` vec.
/// Argv *shape* only — the flag names, never their values.
///
/// Known ceiling: this is a scan, not a parse, so a *value* spelled exactly
/// `--to` or `--to-owner-of` would be miscounted. clap rejects that spelling
/// without a `--` separator anyway, and the cost of being wrong is one
/// mislabelled data point. If `send` ever grows a caller that is not the CLI,
/// pass the mode in instead of guessing at the process's arguments.
fn addressing_from_argv(args: impl Iterator<Item = String>) -> &'static str {
    let (mut literal, mut owner) = (false, false);
    for arg in args {
        // `--to=x` and `--to x` are the same flag to clap, so compare the stem.
        match arg.split('=').next().unwrap_or_default() {
            "--to" => literal = true,
            "--to-owner-of" => owner = true,
            _ => {}
        }
    }
    match (literal, owner) {
        (true, true) => "mixed",
        (false, true) => "to-owner-of",
        _ => "to",
    }
}

fn addressing_mode() -> &'static str {
    addressing_from_argv(std::env::args())
}

/// Age of a message in milliseconds. Clamped at zero: bd's clock and pact's are
/// the same clock today, but a future-dated `created_at` (hand-edited data, a
/// machine whose time jumped) must not become a negative duration. `None` when
/// the stamp does not parse, same as everywhere else in this module.
fn age_ms(created_at: &str, now: DateTime<Utc>) -> Option<f64> {
    let created = parse_ts(created_at)?;
    Some((now - created).num_milliseconds().max(0) as f64)
}

/// Index into [`UNREAD_AGE_BUCKETS`] for an age in seconds.
fn bucket_index(age_secs: i64) -> usize {
    UNREAD_AGE_BUCKETS
        .iter()
        .position(|(_, limit)| age_secs < *limit)
        .unwrap_or(UNREAD_AGE_BUCKETS.len() - 1)
}

/// How many messages are sitting unread for the querying agent, per age bucket.
fn unread_by_bucket(messages: &[Message], now: DateTime<Utc>) -> [i64; UNREAD_AGE_BUCKETS.len()] {
    let mut counts = [0i64; UNREAD_AGE_BUCKETS.len()];
    for m in messages.iter().filter(|m| !m.read) {
        let secs = age_ms(&m.created_at, now).map_or(0, |ms| (ms / 1000.0) as i64);
        counts[bucket_index(secs)] += 1;
    }
    counts
}

/// Report the inbox as queue depth.
///
/// A gauge and not a counter: `pact ui` calls `inbox` on every refresh, and a
/// counter would multiply one rotting message by the number of times the
/// dashboard happened to look at it. "How deep is the queue, and how stale" is
/// a spot measurement.
///
/// Every bucket is emitted, empty ones included. A gauge keeps its last value,
/// so a bucket that simply stopped being reported would read as permanently
/// full — which is the same false alarm this metric exists to avoid raising.
fn record_unread(messages: &[Message], now: DateTime<Utc>) {
    let counts = unread_by_bucket(messages, now);
    for (i, (bucket, _)) in UNREAD_AGE_BUCKETS.iter().enumerate() {
        otel::gauge(
            "pact.msg.unread",
            counts[i],
            &attrs!["pact.msg.age_bucket" => *bucket],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------ the native store
    //
    // Every test below runs with no `bd` anywhere near it. That is not a detail of
    // the fixtures: there is no longer any code path from a message to a
    // subprocess, so a store test cannot accidentally be a backend test.

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        tmp
    }

    fn mail<'a>(body: &'a str) -> Draft<'a> {
        Draft {
            thread: None,
            subject: None,
            body,
            about: &[],
            notice: false,
        }
    }

    #[test]
    fn a_send_lands_in_the_recipients_inbox_and_nowhere_else() {
        let tmp = repo();
        let root = tmp.path();
        let sent = send(
            root,
            "alpha",
            &["bravo".into()],
            Draft {
                subject: Some("the contract"),
                ..mail("MAX_QUADS moved")
            },
        )
        .unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "bravo");
        assert_eq!(sent[0].from, "alpha");
        assert!(!sent[0].read, "a message is not read by being sent");

        let bravo = inbox(root, "bravo", false).unwrap();
        assert_eq!(bravo.len(), 1);
        assert_eq!(bravo[0].body, "MAX_QUADS moved");
        assert_eq!(bravo[0].subject.as_deref(), Some("the contract"));

        assert!(
            inbox(root, "charlie", false).unwrap().is_empty(),
            "an unaddressed agent has an empty inbox, not everyone else's"
        );
    }

    #[test]
    fn one_send_to_many_is_one_row_one_thread_and_one_entry_per_recipient() {
        let tmp = repo();
        let root = tmp.path();
        let sent = send(
            root,
            "alpha",
            &["bravo".into(), "charlie".into()],
            mail("the enum grew a variant"),
        )
        .unwrap();

        // The API fans out, so `--json` still has one entry per recipient and `to` is
        // still a single agent name.
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].to, "bravo");
        assert_eq!(sent[1].to, "charlie");
        assert_eq!(
            sent[0].thread, sent[1].thread,
            "one send is one thread, however many recipients"
        );
        assert_eq!(
            sent[0].id, sent[1].id,
            "and one row, so both copies carry the same id"
        );

        // The STORE holds one line, not one per recipient. That is the whole reason
        // the parent-child fan-out could go.
        let (records, skipped) = records(root).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(skipped, 0);
        assert_eq!(records[0].to, vec!["bravo", "charlie"]);

        // And a reply from either recipient joins that same thread.
        let reply = send(
            root,
            "charlie",
            &["alpha".into()],
            Draft {
                thread: Some(&sent[1].id),
                ..mail("which variant?")
            },
        )
        .unwrap();
        assert_eq!(reply[0].thread, sent[0].thread);

        let thread = read_thread(root, "alpha", &sent[0].id).unwrap();
        let bodies: Vec<&str> = thread.iter().map(|m| m.body.as_str()).collect();
        assert!(
            bodies.contains(&"the enum grew a variant") && bodies.contains(&"which variant?"),
            "the thread holds both halves of the conversation: {bodies:?}"
        );
    }

    #[test]
    fn sending_the_same_message_twice_delivers_it_once() {
        let tmp = repo();
        let root = tmp.path();
        let first = send(root, "alpha", &["bravo".into()], mail("same words")).unwrap();
        let again = send(root, "alpha", &["bravo".into()], mail("same words")).unwrap();

        assert_eq!(
            first[0].id, again[0].id,
            "the id is a hash of the content, so a replay computes the same one"
        );
        let (records, _) = records(root).unwrap();
        assert_eq!(
            records.len(),
            1,
            "the duplicate line is collapsed on read, so bravo is told once"
        );
        assert_eq!(inbox(root, "bravo", false).unwrap().len(), 1);
        assert_eq!(
            records[0].at, first[0].created_at,
            "and the surviving row keeps the FIRST delivery's timestamp"
        );
    }

    /// The property this buys over the bd era, which could not have it: a REPLY is
    /// idempotent too. On bd the deterministic id could only ride a root message,
    /// because a create could not carry `--id` alongside `--parent`.
    #[test]
    fn a_replayed_reply_is_also_delivered_once() {
        let tmp = repo();
        let root = tmp.path();
        let root_msg = send(root, "alpha", &["bravo".into()], mail("question")).unwrap();
        let draft = || Draft {
            thread: Some(&root_msg[0].id),
            ..mail("answer")
        };
        let a = send(root, "bravo", &["alpha".into()], draft()).unwrap();
        let b = send(root, "bravo", &["alpha".into()], draft()).unwrap();
        assert_eq!(a[0].id, b[0].id);
        let (records, _) = records(root).unwrap();
        assert_eq!(records.len(), 2, "the question and one answer");
    }

    #[test]
    fn each_message_chains_to_the_one_before_it() {
        let tmp = repo();
        let root = tmp.path();
        send(root, "alpha", &["bravo".into()], mail("first")).unwrap();
        send(root, "alpha", &["bravo".into()], mail("second")).unwrap();
        send(root, "alpha", &["bravo".into()], mail("third")).unwrap();

        let text = std::fs::read_to_string(messages_file_path(root)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);

        let mut point = crate::events::CHAIN_GENESIS.to_string();
        for (i, line) in lines.iter().enumerate() {
            let mut record: Record = serde_json::from_str(line).unwrap();
            let stored = record.chain_hash.take().expect("every line is chained");
            let canonical = serde_json::to_string(&record).unwrap();
            assert_eq!(
                stored,
                crate::events::chain_hash_of(&point, &canonical),
                "line {} does not chain to the one before it",
                i + 1
            );
            point = stored;
        }
    }

    #[test]
    fn a_torn_final_line_is_counted_and_skipped_without_losing_the_rest() {
        let tmp = repo();
        let root = tmp.path();
        send(root, "alpha", &["bravo".into()], mail("survivor")).unwrap();

        // A half-written append: the shape a write that failed on ENOSPC leaves.
        let path = messages_file_path(root);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"id\":\"pact-msg-000\",\"at\":\"2026-08-13T10:00");
        std::fs::write(&path, text).unwrap();

        let (rows, skipped) = records(root).unwrap();
        assert_eq!(skipped, 1, "the torn line is counted, not silently dropped");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "survivor");

        // And the next append does not glue itself onto the dangling offset.
        send(root, "alpha", &["bravo".into()], mail("after the tear")).unwrap();
        let (rows, skipped) = records(root).unwrap();
        assert_eq!(skipped, 1, "still one bad line, not two");
        assert_eq!(rows.len(), 2);
    }

    // -------------------------------------------------------- read cursors

    #[test]
    fn reading_a_thread_marks_it_for_that_reader_alone() {
        let tmp = repo();
        let root = tmp.path();
        let sent = send(
            root,
            "alpha",
            &["bravo".into(), "charlie".into()],
            mail("both of you"),
        )
        .unwrap();

        let shown = read_thread(root, "bravo", &sent[0].id).unwrap();
        assert!(
            shown.iter().all(|m| m.read),
            "the reader sees it as read in the same call that recorded it"
        );

        // bravo's copy is read; charlie's is not. One row, two independent read
        // positions — the property the per-recipient beads used to provide.
        let bravo = inbox(root, "bravo", false).unwrap();
        assert!(bravo[0].read);
        let charlie = inbox(root, "charlie", false).unwrap();
        assert!(!charlie[0].read, "charlie has not read anything");
        assert!(
            inbox(root, "charlie", true).unwrap().len() == 1,
            "so it is still in charlie's unread queue"
        );
    }

    /// The one thing local read state must still do, and the reason pact-rnc.17
    /// moved it into bd in the first place: a sender can see that their message
    /// landed. Within a shared checkout — which is pact's model — a cursor written by
    /// one agent is readable by another, so `sent` can still answer honestly.
    #[test]
    fn a_sender_sees_that_the_recipient_read_it() {
        let tmp = repo();
        let root = tmp.path();
        let posted = send(root, "alpha", &["bravo".into()], mail("please confirm")).unwrap();

        let before = sent(root, "alpha").unwrap();
        assert!(!before[0].read, "unread until somebody reads it");
        assert!(before[0].read_by.is_empty());

        read_thread(root, "bravo", &posted[0].id).unwrap();

        let after = sent(root, "alpha").unwrap();
        assert_eq!(after[0].read_by, vec!["bravo"]);
        assert!(
            after[0].read,
            "`read` on a sender's own listing resolves against the recipient"
        );
        assert!(
            unacknowledged(root).unwrap().is_empty(),
            "and the message stops being reported as unacknowledged"
        );
    }

    #[test]
    fn re_reading_a_thread_is_idempotent_and_does_not_duplicate_a_reader() {
        let tmp = repo();
        let root = tmp.path();
        let sent = send(root, "alpha", &["bravo".into()], mail("twice")).unwrap();
        read_thread(root, "bravo", &sent[0].id).unwrap();
        let second = read_thread(root, "bravo", &sent[0].id).unwrap();
        assert_eq!(second[0].read_by, vec!["bravo"]);
    }

    #[test]
    fn a_malformed_read_cursor_reads_as_having_read_nothing() {
        let tmp = repo();
        let root = tmp.path();
        let sent = send(root, "alpha", &["bravo".into()], mail("hello")).unwrap();
        read_thread(root, "bravo", &sent[0].id).unwrap();

        std::fs::write(cursor_path(root, "bravo"), "{not json").unwrap();
        let bravo = inbox(root, "bravo", false).unwrap();
        assert!(
            !bravo[0].read,
            "local, regenerable state: unreadable means 'read nothing', never an error"
        );
    }

    /// The property the MCP server depends on: looking does not deliver. An observer
    /// that marked threads read would tell every sender their message had landed.
    #[cfg(feature = "mcp")]
    #[test]
    fn peeking_a_thread_leaves_it_unread() {
        let tmp = repo();
        let root = tmp.path();
        let posted = send(root, "alpha", &["bravo".into()], mail("do not ack me")).unwrap();

        let peeked = peek_thread(root, Some("bravo"), &posted[0].id).unwrap();
        assert_eq!(peeked.len(), 1);
        assert!(!peeked[0].read);

        assert!(
            !inbox(root, "bravo", false).unwrap()[0].read,
            "peeking must not write a read cursor"
        );
        assert_eq!(
            sent(root, "alpha").unwrap()[0].read_by,
            Vec::<String>::new(),
            "and the sender must not be told it landed"
        );
    }

    // ------------------------------------------------------ addressing an id

    #[test]
    fn an_id_prefix_is_enough_to_read_a_thread() {
        let tmp = repo();
        let root = tmp.path();
        let sent = send(root, "alpha", &["bravo".into()], mail("prefix me")).unwrap();
        let short = &sent[0].id[..14];
        let shown = read_thread(root, "bravo", short).unwrap();
        assert_eq!(shown[0].body, "prefix me");
    }

    #[test]
    fn an_ambiguous_id_prefix_names_the_candidates_instead_of_guessing() {
        let tmp = repo();
        let root = tmp.path();
        send(root, "alpha", &["bravo".into()], mail("one")).unwrap();
        send(root, "alpha", &["bravo".into()], mail("two")).unwrap();

        // Every id shares the `pact-msg-` prefix, so that alone is ambiguous.
        let err = read_thread(root, "bravo", "pact-msg-").unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("matches 2 messages"),
            "an ambiguous prefix must say so: {text}"
        );
        assert!(
            text.contains("use more of the id"),
            "and say what to do about it: {text}"
        );
    }

    #[test]
    fn an_unknown_id_is_an_error_and_not_an_empty_thread() {
        let tmp = repo();
        let err = read_thread(tmp.path(), "bravo", "pact-msg-nope").unwrap_err();
        assert!(format!("{err:#}").contains("no message matching"));
    }

    // -------------------------------------------------------- about a path

    #[test]
    fn a_message_about_a_path_is_found_by_that_path() {
        let tmp = repo();
        let root = tmp.path();
        send(
            root,
            "alpha",
            &["bravo".into()],
            Draft {
                about: &["src/ast.rs".to_string()],
                ..mail("the visitor signature changed")
            },
        )
        .unwrap();
        send(root, "alpha", &["bravo".into()], mail("unrelated traffic")).unwrap();

        let about = about_path(root, "src/ast.rs").unwrap();
        assert_eq!(about.len(), 1);
        assert_eq!(about[0].body, "the visitor signature changed");
        assert!(
            about_path(root, "src/other.rs").unwrap().is_empty(),
            "another path's traffic is not this path's"
        );
    }

    /// Raw paths, where bd labels needed an encoding that turned every `.` into a
    /// `-`. A dot is in every real filename, so this is the common case, not an edge
    /// one — and `a.b` and `a-b` used to collapse onto the same tag.
    #[test]
    fn two_paths_that_the_old_label_encoding_collapsed_stay_distinct() {
        let tmp = repo();
        let root = tmp.path();
        for path in ["src/a.rs", "src/a-rs"] {
            send(
                root,
                "alpha",
                &["bravo".into()],
                Draft {
                    about: &[path.to_string()],
                    ..mail(path)
                },
            )
            .unwrap();
        }
        let hits = about_path(root, "src/a.rs").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].body, "src/a.rs");
    }

    // ------------------------------------------------------------- notices

    #[test]
    fn a_notice_is_classified_by_its_stored_kind() {
        let tmp = repo();
        let root = tmp.path();
        send(
            root,
            "alpha",
            &["bravo".into()],
            Draft {
                subject: Some(&format!("src/ast.rs{NOTICE_SUBJECT_MARKER}alpha")),
                notice: true,
                ..mail("diff follows")
            },
        )
        .unwrap();
        let bravo = inbox(root, "bravo", false).unwrap();
        assert!(bravo[0].notice);
        let (records, _) = records(root).unwrap();
        assert_eq!(records[0].kind, NOTICE);
    }

    /// An agent writing prose that happens to look like a notice is correspondence,
    /// and this is now true by construction rather than by a three-condition
    /// heuristic over English: the kind is a field, and no CLI path sets it.
    #[test]
    fn prose_that_reads_like_a_notice_is_still_correspondence() {
        let tmp = repo();
        let root = tmp.path();
        send(
            root,
            "alpha",
            &["bravo".into()],
            Draft {
                subject: Some(&format!(
                    "src/ast.rs{NOTICE_SUBJECT_MARKER}alpha, please re-read it"
                )),
                ..mail("src/ast.rs changed, which you are watching.")
            },
        )
        .unwrap();
        let bravo = inbox(root, "bravo", false).unwrap();
        assert!(
            !bravo[0].notice,
            "a message no watch wrote must not be filed as machine noise"
        );
    }

    /// pact-rnc.20's ordering rule, still load-bearing: a store can hold lines from
    /// both eras, bd's `Z` stamps and pact's own `+00:00` ones, and `'+'` (0x2B)
    /// sorts before `'Z'` (0x5A) — so a string compare calls an older `Z` stamp
    /// newer than a `+00:00` one.
    #[test]
    fn sorting_compares_instants_and_not_raw_stamps() {
        let mut messages = [authored_msg("newer-offset"), authored_msg("older-zulu")];
        messages[0].created_at = "2026-08-13T12:00:00+00:00".to_string();
        messages[1].created_at = "2026-08-13T09:00:00Z".to_string();
        messages.sort_by(oldest_first);
        assert_eq!(
            messages[0].id, "older-zulu",
            "the earlier instant sorts first regardless of how it spells its zone"
        );
    }

    #[test]
    fn an_unparsable_stamp_sorts_oldest_rather_than_panicking() {
        let mut messages = [authored_msg("good"), authored_msg("bad")];
        messages[0].created_at = "2026-08-13T09:00:00Z".to_string();
        messages[1].created_at = "not a timestamp".to_string();
        messages.sort_by(oldest_first);
        assert_eq!(messages[0].id, "bad");
    }

    #[test]
    fn send_with_no_recipients_is_an_error() {
        let tmp = repo();
        let err = send(tmp.path(), "alpha", &[], mail("into the void")).unwrap_err();
        assert!(format!("{err:#}").contains("no recipients"));
    }

    #[test]
    fn a_message_id_is_stable_across_repeated_calls() {
        let to = vec!["bravo".to_string()];
        let a = message_id("alpha", &to, None, "subject", "body");
        let b = message_id("alpha", &to, None, "subject", "body");
        assert_eq!(a, b, "the id is a pure function of the send's own content");
        assert!(a.starts_with("pact-msg-"));
    }

    #[test]
    fn a_message_id_differs_when_any_input_differs() {
        let to = vec!["bravo".to_string()];
        let base = message_id("alpha", &to, None, "subject", "body");
        let others = [
            message_id("other", &to, None, "subject", "body"),
            message_id("alpha", &["charlie".to_string()], None, "subject", "body"),
            message_id("alpha", &to, Some("pact-msg-root"), "subject", "body"),
            message_id("alpha", &to, None, "different", "body"),
            message_id("alpha", &to, None, "subject", "different"),
        ];
        for other in others {
            assert_ne!(base, other);
        }
    }

    /// The recipient LIST is part of the id, so adding a recipient is a different
    /// message rather than a replay of the smaller send.
    #[test]
    fn adding_a_recipient_makes_it_a_different_message() {
        let one = message_id("alpha", &["bravo".to_string()], None, "s", "b");
        let two = message_id(
            "alpha",
            &["bravo".to_string(), "charlie".to_string()],
            None,
            "s",
            "b",
        );
        assert_ne!(one, two);
    }

    fn notice_msg(id: &str, path: &str, holder: &str, at: &str, read: bool) -> Message {
        Message {
            id: id.into(),
            thread: id.into(),
            from: holder.into(),
            to: "watcher".into(),
            subject: Some(format!("{path}{NOTICE_SUBJECT_MARKER}{holder}")),
            body: "a diff".into(),
            created_at: at.into(),
            read,
            read_by: Vec::new(),
            notice: true,
        }
    }

    fn authored_msg(id: &str) -> Message {
        Message {
            notice: false,
            subject: Some("six duplicate test fns in src/parser.rs".into()),
            ..notice_msg(id, "unused", "agent-05", "2026-08-11T09:30:55Z", false)
        }
    }

    #[test]
    fn default_subject_uses_first_line_truncated() {
        assert_eq!(default_subject("hello\nworld"), "hello");
        assert_eq!(default_subject(""), "(no subject)");
        assert_eq!(default_subject("   \nignored"), "(no subject)");
        let long = "x".repeat(80);
        let subject = default_subject(&long);
        assert_eq!(subject.chars().count(), 60);
        assert!(subject.ends_with("..."));
    }

    /// pact-aw7.4: the counter that says whether the fleet adopted
    /// `--to-owner-of` or kept addressing agents that had already exited.
    #[test]
    fn addressing_mode_reads_the_flag_and_not_its_value() {
        let argv = |s: &str| {
            addressing_from_argv(
                s.split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        };
        assert_eq!(argv("pact msg send --to docs-writer body"), "to");
        assert_eq!(argv("pact msg send --to=docs-writer body"), "to");
        assert_eq!(
            argv("pact msg send --to-owner-of src/msg.rs body"),
            "to-owner-of"
        );
        assert_eq!(
            argv("pact msg send --to-owner-of=src/msg.rs body"),
            "to-owner-of"
        );
        assert_eq!(
            argv("pact msg send --to human --to-owner-of src/msg.rs b"),
            "mixed"
        );
        // Whatever argv holds, exactly three labels ever reach the collector.
        for line in [
            "pact msg send --to a body",
            "pact msg send --to-owner-of p b",
            "pact msg send --to a --to-owner-of p b",
            "pact msg inbox",
        ] {
            assert!(["to", "to-owner-of", "mixed"].contains(&argv(line)));
        }
    }

    /// pact-aw7.4: 38 minutes is the number that mattered, and the bucket it
    /// lands in is the whole reason this is an attribute and not the shared
    /// 10 s-max histogram.
    #[test]
    fn unread_age_buckets_span_the_timescale_coordination_fails_on() {
        let names: Vec<&str> = [0, 59, 60, 299, 300, 899, 900, 2280, 3599, 3600, 86_400]
            .iter()
            .map(|s| UNREAD_AGE_BUCKETS[bucket_index(*s)].0)
            .collect();
        assert_eq!(
            names,
            [
                "lt_1m", "lt_1m", "1m_5m", "1m_5m", "5m_15m", "5m_15m", "15m_1h",
                "15m_1h", // 2280 s = the 38-minute BLOCKER
                "15m_1h", "gt_1h", "gt_1h",
            ]
        );
    }

    #[test]
    fn unread_counting_ignores_read_mail_and_survives_a_skewed_clock() {
        let now = parse_ts("2026-08-02T12:00:00Z").unwrap();
        let at = |ts: &str, read: bool| Message {
            id: "m".into(),
            thread: "m".into(),
            from: "peer".into(),
            to: "msg-metrics".into(),
            subject: None,
            body: String::new(),
            created_at: ts.to_string(),
            read,
            read_by: Vec::new(),
            notice: false,
        };
        let counts = unread_by_bucket(
            &[
                at("2026-08-02T11:59:30Z", false), // 30 s
                at("2026-08-02T11:22:00Z", false), // 38 min — the BLOCKER
                at("2026-08-02T09:00:00Z", false), // 3 h
                at("2026-08-02T09:00:00Z", true),  // read: not queue depth
                at("2026-08-02T12:05:00Z", false), // future-dated: clamps to 0
                at("not a timestamp", false),      // unparsable: clamps to 0
            ],
            now,
        );
        assert_eq!(
            counts,
            [3, 0, 0, 1, 1],
            "lt_1m, 1m_5m, 5m_15m, 15m_1h, gt_1h"
        );

        // And the age a read would report, in the milliseconds record_ms wants.
        assert_eq!(age_ms("2026-08-02T11:22:00Z", now), Some(2_280_000.0));
        assert_eq!(age_ms("2026-08-02T12:05:00Z", now), Some(0.0));
        assert_eq!(age_ms("not a timestamp", now), None);
    }

    /// The crucible shape: nine deliveries of one path in nine seconds. One
    /// entry, count 9, and the LATEST releaser — the only one whose diff is
    /// still current.
    #[test]
    fn notices_for_one_path_coalesce_and_keep_the_latest_releaser() {
        let msgs: Vec<Message> = (0..9)
            .map(|i| {
                notice_msg(
                    &format!("pact-msg-{i}"),
                    "src/ast.rs",
                    &format!("agent-0{i}"),
                    &format!("2026-08-11T09:30:5{i}Z"),
                    false,
                )
            })
            .collect();
        let (authored, groups) = split_notices(&msgs);
        assert!(authored.is_empty());
        assert_eq!(groups.len(), 1, "one path, one entry: {groups:?}");
        assert_eq!(groups[0].path, "src/ast.rs");
        assert_eq!(groups[0].count, 9);
        assert_eq!(groups[0].unread, 9);
        assert_eq!(groups[0].latest_from, "agent-08");
        assert_eq!(groups[0].latest_id, "pact-msg-8");
    }

    /// The one authored message in that window must survive the split intact —
    /// it is the whole reason for the split.
    #[test]
    fn the_authored_message_is_never_grouped_away() {
        let msgs = vec![
            notice_msg(
                "n1",
                "src/ast.rs",
                "agent-01",
                "2026-08-11T09:30:50Z",
                false,
            ),
            authored_msg("a1"),
            notice_msg("n2", "src/ast.rs", "agent-02", "2026-08-11T09:30:52Z", true),
            notice_msg(
                "n3",
                "src/eval.rs",
                "agent-09",
                "2026-08-11T09:30:53Z",
                false,
            ),
        ];
        let (authored, groups) = split_notices(&msgs);
        assert_eq!(authored.len(), 1);
        assert_eq!(authored[0].id, "a1");
        // First-notice order, so a listing does not reshuffle between runs.
        assert_eq!(
            groups.iter().map(|g| g.path.as_str()).collect::<Vec<_>>(),
            ["src/ast.rs", "src/eval.rs"]
        );
        // Read state is per notice, not per group.
        assert_eq!((groups[0].count, groups[0].unread), (2, 1));
        assert_eq!((groups[1].count, groups[1].unread), (1, 1));
    }

    /// A freed notice (pact-bsf) carries the OTHER marker, and must group under
    /// its path just like a diff notice — otherwise every mutex release would
    /// group under its whole subject and a waiter's inbox would list one entry
    /// per release instead of one per path.
    #[test]
    fn a_freed_notice_groups_under_its_path_like_a_diff_notice() {
        let mutex = ".pact/internal/merge-to-master";
        let mut freed = notice_msg("n1", "x", "sluice", "2026-08-15T17:08:43Z", false);
        freed.subject = Some(format!("{mutex}{NOTICE_FREED_MARKER}sluice"));
        let mut freed2 = notice_msg("n2", "x", "fuller", "2026-08-15T17:09:10Z", false);
        freed2.subject = Some(format!("{mutex}{NOTICE_FREED_MARKER}fuller"));

        let (_, groups) = split_notices(&[freed, freed2]);
        assert_eq!(groups.len(), 1, "one path, one group: {groups:?}");
        assert_eq!(groups[0].path, mutex);
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].latest_from, "fuller", "the latest releaser wins");
    }

    /// The two markers must not collide: a diff notice still parses with the
    /// freed marker in the table, and vice versa.
    #[test]
    fn both_notice_markers_parse_their_own_path() {
        assert_eq!(
            notice_path(&format!("src/api.rs{NOTICE_SUBJECT_MARKER}alpha")),
            "src/api.rs"
        );
        assert_eq!(
            notice_path(&format!("src/api.rs{NOTICE_FREED_MARKER}alpha")),
            "src/api.rs"
        );
    }

    /// Drift must degrade the grouping, never lose a message. A notice whose
    /// subject no longer carries the marker groups under its whole subject and
    /// is still counted.
    #[test]
    fn a_notice_with_an_unparsable_subject_still_counts() {
        let mut odd = notice_msg("n1", "x", "agent-01", "2026-08-11T09:30:50Z", false);
        odd.subject = Some("something else entirely".into());
        let (_, groups) = split_notices(&[odd]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].path, "something else entirely");
        assert_eq!(groups[0].count, 1);
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".pact")).unwrap();
        tmp
    }

    /// pact-e7d: a handoff is capped, and says so rather than trailing off.
    ///
    /// The bound matters because of who writes these: an agent that has just
    /// finished something, with all of it still in context, asked for "findings".
    /// That is the exact condition that produces a wall of text — and past a point
    /// a wall is not a message but a pointer to one, which is the measurement
    /// `watch`'s diff cap already carries.
    ///
    /// Naming the cap in the text is the load-bearing half: silence and brevity
    /// look identical at the bottom of a message, so a reader must be able to tell
    /// a truncated handoff from a terse one.
    #[test]
    fn an_overlong_handoff_is_capped_and_says_where_the_rest_went() {
        let tmp = repo();
        let long: String = (0..500)
            .map(|i| format!("finding {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        let r = post_to_thread(tmp.path(), "finisher", "bead:m-bbb", "m-aaa", "s", &long).unwrap();

        let lines = r.body.lines().count();
        assert!(lines < 500, "it must actually be cut: {lines}");
        assert!(r.body.contains("finding 0"), "the top survives");
        assert!(!r.body.contains("finding 499"), "the tail does not");
        assert!(
            r.body.contains("truncated") && r.body.contains("500"),
            "and it must name the cap and the original size: {}",
            r.body.lines().next_back().unwrap_or_default()
        );
    }

    /// The thread is the address, and the source bead is a FIELD.
    ///
    /// `handoff_from` exists so `pact audit`'s coverage arithmetic does not have
    /// to regex `handoff from <id>` out of a subject line anybody could reword —
    /// the same reason `Event::bead` is a field rather than a substring of a note.
    #[test]
    fn a_handoff_records_who_it_is_from_and_who_it_is_for() {
        let tmp = repo();
        let r = post_to_thread(tmp.path(), "finisher", "bead:m-bbb", "m-aaa", "s", "body").unwrap();

        assert_eq!(r.thread, "bead:m-bbb", "addressed to the WORK");
        assert_eq!(r.handoff_from.as_deref(), Some("m-aaa"));
        assert_eq!(r.kind, HANDOFF);
        assert!(
            r.to.is_empty(),
            "no recipient: the inheritor may not exist yet"
        );

        // And it is reachable only through the thread — `all_messages` fans out
        // per recipient, so a record with none produces nothing at all.
        assert!(all_messages(tmp.path()).unwrap().is_empty());
        assert_eq!(thread_records(tmp.path(), "bead:m-bbb").unwrap().len(), 1);
    }
}
