//! Threaded messaging, layered on `bd create --type=message` +
//! `--parent`/`--assignee`/`--include-infra`.
//!
//! Flags were confirmed empirically against a scratch `bd` database rather
//! than assumed: `bd create --type=message` works (message is a real, if
//! undocumented in `--type`'s help text, issue type used for "infra" beads);
//! `bd show --thread` does NOT aggregate parent-child replies in this bd
//! version (it only ever prints the single issue), so thread reconstruction
//! is done ourselves via `bd list --parent <id> --include-infra --json`
//! (which does correctly return the children) instead of relying on it.
//! `bd list` has no `--type` filter, so filtering to `issue_type == "message"`
//! happens client-side.
//!
//! Read state lives in bd, as one `read-by-<agent>` label per reader
//! (pact-rnc.17). It used to be `.pact/read.json`, per-agent *local* state,
//! which meant a sender structurally could not see whether anyone had read
//! their message. Labels are shared, so they can: `Message::read_by` lists
//! every reader and `read` is just "read_by contains the querying agent".
//! There is no local read state any more, hence no gitignore rule to manage
//! (a leftover `.pact/read.json` from an older pact is inert, and the single
//! `.pact/` gitignore line from `agents_md` covers it anyway).
//!
//! Verified against bd 1.1.0: `bd list/show --json` hydrate `labels` (there is
//! a `--skip-labels` to turn that off, which pact never passes), `bd label add`
//! takes several ids at once and is idempotent — and a child bead *inherits*
//! its parent's labels unless `--no-inherit-labels` is passed, which is why
//! every create here passes it. Without it a reply to a message you had
//! already read would be born carrying your own `read-by-` label.
//!
//! # br (beads-rust)
//!
//! br 0.2.19 runs the same model but not the same argv, so the four places
//! below branch on [`BeadsCli::is_br`]. Every claim here was checked by running
//! the binary against a scratch workspace (pact-l94), not read off bd's docs:
//!
//! - `br create --type=message --title= --description= --assignee= --parent=
//!   --actor= --json` all work unchanged, and return the same single JSON
//!   object bd does.
//! - `--no-inherit-labels` does not exist (`error: unexpected argument`) and is
//!   not needed: a br child is born with no labels at all, so the bug that flag
//!   exists to prevent cannot happen. It is therefore omitted, not faked.
//! - `--include-infra` does not exist either, and is equally unnecessary: plain
//!   `br list` already returns `issue_type: "message"` beads. br *does* have the
//!   `--type` filter bd lacks, so the filtering happens server-side there and
//!   client-side for bd.
//! - `br list --json` returns an envelope, `{"issues":[…],"total":…}`, where bd
//!   returns a bare array — hence [`parse_issues`] accepts either.
//! - `br list --json` omits `parent`, and `br list` has no `--parent` filter, so
//!   neither the thread column nor `read_thread`'s reply fetch can come from it.
//!   `br show <id>… --json` *does* carry `parent`, so a thread's root is found
//!   the same way bd finds it. Its replies are NOT read from that same `show`
//!   response's `dependents` field, though — that field is a snapshot from
//!   whenever the root was fetched, and a reply created after the fetch stayed
//!   invisible until something re-fetched the root (pact-m7j.6.1). `br dep list
//!   <root> --direction up --json` answers the same question as its own fresh
//!   query, so it is asked every time — one extra subprocess (`dep list` for
//!   the ids, `show` for the records) in exchange for the same data bd gives,
//!   from the backend itself rather than from guessing at br's `<id>.<n>` id
//!   shape.

use std::cmp::Ordering;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::beads::BeadsCli;
use crate::output;
use crate::{attrs, otel};

/// Label prefix marking "this agent has read this message".
const READ_BY: &str = "read-by-";

/// Label marking a message as being ABOUT a path, so delivery can follow the
/// file instead of stopping at the name the path resolved to.
///
/// `--to-owner-of` addressed a file and then resolved it to an agent, and
/// delivery stopped there. Measured over one fleet run: 30 of 44 agent-to-agent
/// messages went to agents who had already exited, and none of those were ever
/// read, while every message to a live agent was read. Addressing was never the
/// failure — deliverability was. Every one of the 30 was about a file, sent to
/// the agent who had just released it (pact-4tj).
const ABOUT: &str = "about-";

/// A path as a label-safe token: `/` to `__`, the same convention lease lock
/// files use — but, unlike a lock filename, this token is never written to a
/// filesystem, only compared against itself inside a bd/br label. So it goes
/// one step further: every byte outside `[A-Za-z0-9_:-]` (that still leaves
/// `/`, just replaced above) becomes `-`. Confirmed necessary against a real
/// br 0.2.19 store: `br create --labels`/`br label add` both reject a `.` with
/// "invalid characters (only alphanumeric, hyphen, underscore, colon
/// allowed)" — exit 4 — which meant every real file path (anything with an
/// extension) silently failed to tag on br, before this. bd tolerates the
/// original punctuation fine, so this narrower charset is a bd-compatible
/// subset, not a second encoding to keep in sync.
///
/// Not reversible, and does not need to be: nothing ever decodes a label back
/// into a path, only re-encodes a query path the same way and compares. Two
/// distinct paths differing only in the punctuation this collapses (`a.b` and
/// `a-b`, say) would produce the same label — the same class of accepted
/// collision the `/` → `__` step already carries, and unlikely in a real tree.
fn encode_path(path: &str) -> String {
    path.replace('/', "__")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// How far [`walk_to_root`] follows `parent` links before giving up. pact only
/// ever creates depth-1 threads; the cap exists so hand-edited or cyclic parent
/// data cannot spin forever, not because deep threads are expected.
const MAX_THREAD_DEPTH: usize = 16;

#[derive(Debug, Serialize)]
pub struct Message {
    pub id: String,
    pub thread: String,
    /// bd's `created_by`: whoever bd recorded as the author. That is the pact
    /// agent name when `send()` passed `--actor` ("tui-dev"), but a git user
    /// name ("Ada Lovelace") for beads created outside pact, so it is passed
    /// through verbatim and is NOT guaranteed to be a pact identity. Empty
    /// string when bd reports no author.
    pub from: String,
    pub to: String,
    pub subject: Option<String>,
    pub body: String,
    pub created_at: String,
    /// Read by the querying agent (for `all_messages()`, which has no querying
    /// agent, read by its own recipient).
    pub read: bool,
    /// Every agent that has read this message, from its `read-by-` labels.
    pub read_by: Vec<String>,
}

/// The subset of `bd`'s issue JSON we care about.
#[derive(Debug, Deserialize)]
struct BdIssue {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    created_by: Option<String>,
    created_at: String,
    #[serde(default)]
    issue_type: String,
    #[serde(default)]
    parent: Option<String>,
    /// bd emits `"labels": null` for an unlabelled bead, so this is an Option
    /// rather than a `#[serde(default)] Vec` (which would fail on null).
    #[serde(default)]
    labels: Option<Vec<String>>,
}

/// One edge from br's `dep list --direction up --json` — the beads that point
/// *at* the queried one. Field names differ from `show`'s own embedded
/// `dependents` (`id`/`dependency_type`): `dep list` calls the same two things
/// `issue_id`/`type` (confirmed against a real `br dep list`, pact-m7j.6.1).
/// bd never emits these.
#[derive(Debug, Deserialize)]
struct DepListItem {
    issue_id: String,
    #[serde(default, rename = "type")]
    dependency_type: String,
}

/// `list --json` output. bd returns a bare array, br wraps it in an envelope;
/// untagged means neither backend needs a parser of its own.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ListPayload {
    Bare(Vec<BdIssue>),
    Envelope { issues: Vec<BdIssue> },
}

impl ListPayload {
    fn into_issues(self) -> Vec<BdIssue> {
        match self {
            ListPayload::Bare(v) | ListPayload::Envelope { issues: v } => v,
        }
    }
}

impl BdIssue {
    /// The one BdIssue -> Message mapping, shared by every read path so `from`
    /// and `read_by` cannot go missing on just one of them. `thread` pins the
    /// thread id (read_thread pins every row to the root); otherwise it is the
    /// parent, falling back to the message's own id for a thread root.
    /// `viewer` is the agent asking; `None` means "resolve `read` against each
    /// message's own recipient", which is what a recipient-agnostic listing
    /// wants.
    fn into_message(self, thread: Option<&str>, viewer: Option<&str>) -> Message {
        let read_by: Vec<String> = self
            .labels
            .unwrap_or_default()
            .iter()
            .filter_map(|l| l.strip_prefix(READ_BY).map(str::to_string))
            .collect();
        let to = self.assignee.unwrap_or_default();
        let read = read_by.iter().any(|a| a == viewer.unwrap_or(&to));
        Message {
            thread: thread
                .map(str::to_string)
                .or(self.parent)
                .unwrap_or_else(|| self.id.clone()),
            from: self.created_by.unwrap_or_default(),
            to,
            subject: Some(self.title),
            body: self.description.unwrap_or_default(),
            created_at: self.created_at,
            id: self.id,
            read,
            read_by,
        }
    }
}

/// Send one message to one or more recipients (pact-rnc.4).
///
/// bd assigns exactly one assignee per bead, so N recipients means N beads —
/// but they are made into ONE readable thread instead of N unrelated ones:
/// recipients 2..N are created as children of the thread root (the first
/// recipient's bead for a new thread, or `thread` itself when this send is a
/// reply), which is exactly how replies already work, so `read_thread()` shows
/// the whole announcement as one conversation. Children of the root rather than
/// of each other because `read_thread()` returns *direct* children only —
/// grandchildren would be invisible in the thread a reader actually opens,
/// which is the bug this fixes.
///
/// Returns one Message per recipient, root first. An empty recipient list is an
/// error. Not atomic — bd has no transaction across N creates — so a failure
/// part-way through leaves the earlier recipients' messages sent; the error is
/// a [`SendFailure`] naming exactly which (`sent()` also lists them), and a
/// caller with `--json` can retry with `--skip` for those recipients instead
/// of re-sending to them blind (pact-m7j.6.5).
/// Everything about a message except who it goes to.
///
/// A struct rather than four more parameters: `send` was already at the limit,
/// and "the message" is a real thing with parts, not an argument list.
pub struct Draft<'a> {
    /// Reply within an existing thread; `None` starts a new one.
    pub thread: Option<&'a str>,
    pub subject: Option<&'a str>,
    pub body: &'a str,
    /// Paths this message is ABOUT, from `--to-owner-of`. Recorded as labels so
    /// delivery can follow the file after the agent it resolved to has exited.
    pub about: &'a [String],
}

/// `send()` failed partway through a multi-recipient fan-out (pact-m7j.6.5).
/// `already_sent` is exactly the recipients who already got this message —
/// re-sending the identical command without `--skip` for them duplicates
/// delivery, since only the thread ROOT (the first recipient) is protected by
/// [`idempotency_key`]'s upsert; recipients 2..N are not. Exposed as
/// `--json`'s error shape so a caller can retry with `--skip <agent>` for each
/// name here instead of parsing prose, per this session's design decision to
/// keep messaging's retry story flag-based rather than content-addressed (the
/// same choice pact-m7j.6.4 made for the single-recipient case).
///
/// `reason` is a one-shot text snapshot of the underlying failure, for a
/// `--json` reader who wants "why" without a second command. It is NOT how the
/// human-readable path learns that text: `send()` attaches this struct via
/// `anyhow::Error::context`, not `map_err`, so the original error survives as
/// this error's source — `{:#}` prints it via the normal chain, and, more
/// importantly, `output::code_for`'s `downcast_ref::<ExitError>()` still finds
/// a nested `ExitError` (a bare-repo topology, a missing backend) through that
/// chain. Replacing the error outright silently downgraded every such failure
/// to the generic exit code 1, which broke `pact`'s documented exit-code
/// contract for exactly the callers it exists to help.
#[derive(Debug, Serialize)]
pub struct SendFailure {
    pub already_sent: Vec<String>,
    pub failed_at: String,
    pub reason: String,
}

impl std::fmt::Display for SendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.already_sent.is_empty() {
            write!(f, "sending to {}: nothing was sent", self.failed_at)
        } else {
            write!(
                f,
                "sending to {}: {} recipient(s) already got this ({}) — replay with \
                 --skip for them instead of re-sending blind",
                self.failed_at,
                self.already_sent.len(),
                self.already_sent.join(", "),
            )
        }
    }
}

impl std::error::Error for SendFailure {}

/// If `err` is a [`SendFailure`], its `--json` shape — for a caller that wants
/// to retry with `--skip` rather than parse the human-readable text. `None`
/// for every other error, so a generic failure still prints as plain text.
pub fn json_send_failure(err: &anyhow::Error) -> Option<String> {
    serde_json::to_string_pretty(err.downcast_ref::<SendFailure>()?).ok()
}

pub fn send(
    cli: &BeadsCli,
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
    } = draft;
    if to.is_empty() {
        anyhow::bail!("no recipients — `msg send` needs at least one --to");
    }
    // One recipient named twice is one recipient. Without this, `--to a --to a`
    // created two beads in one thread and delivered the same message to the
    // same inbox twice — reproducible, and silent, because pact has no
    // uniqueness constraint to trip over. Agent Mail hit the same case hard
    // enough to get a composite-primary-key IntegrityError and fixed it by
    // deduping before building the recipient rows (c66e54f, #190).
    //
    // The realistic caller is not a human typing the flag twice: the protocol
    // block tells agents to repeat `--to` for a multi-recipient decision, so a
    // list built from `pact agents --json` or an orchestrator template can
    // repeat a name. And `pact msg sent` exists precisely because a previous
    // fleet produced duplicate messages, so a command that manufactures them
    // works against the tool's own advice.
    //
    // First-seen order is preserved: the printed thread root must not move
    // because a later duplicate was dropped.
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<String> = to.iter().filter(|r| seen.insert(*r)).cloned().collect();
    let dropped = to.len() - deduped.len();
    if dropped > 0 {
        // Said out loud, not swallowed: a caller that repeated a name probably
        // built the list wrongly and should find out now.
        output::warn(&format!(
            "note: {dropped} duplicate recipient(s) collapsed — sending one message per distinct agent"
        ));
    }
    let to: &[String] = &deduped;
    let title = subject
        .map(str::to_string)
        .unwrap_or_else(|| default_subject(body));

    // Parent for the next bead: the caller's thread if this is a reply,
    // otherwise (first iteration) nothing — and from then on the root's id, so
    // recipients 2..N hang off the same bead `read_thread` is asked about.
    //
    // `thread` is resolved to the thread ROOT rather than used verbatim, because
    // an agent legitimately holds a non-root member id: `msg send` prints one id
    // per recipient, and recipients 2..N of a fan-out see their own child id.
    // Parenting a reply on one of those makes a grandchild, and `read_thread`
    // returns direct children only — so the reply is invisible to everyone
    // reading the thread, which is exactly the fragmentation pact-rnc.4 exists
    // to prevent.
    let mut thread_id = match thread {
        Some(t) => Some(thread_root(cli, repo_root, t)?.id),
        None => None,
    };
    let mut messages: Vec<Message> = Vec::with_capacity(to.len());
    // Read once, not per recipient: it is a property of the command line, and
    // every bead this send creates was addressed the same way.
    let addressing = addressing_mode();
    let is_reply = thread.is_some();
    for recipient in to {
        let issue = match create(
            cli,
            repo_root,
            agent,
            recipient,
            thread_id.as_deref(),
            &title,
            body,
            about,
        ) {
            Ok(issue) => issue,
            Err(e) => {
                // Captured before `e` is moved into `context`: the source
                // chain still carries this text (and anything under it, like
                // an `ExitError`'s code), but the JSON shape wants it as a
                // plain string, not something a reader has to re-derive from
                // the chain.
                let reason = format!("{e:#}");
                return Err(e.context(SendFailure {
                    already_sent: messages.iter().map(|m| m.to.clone()).collect(),
                    failed_at: recipient.clone(),
                    reason,
                }));
            }
        };
        let id = issue.id;
        let thread = thread_id.get_or_insert_with(|| id.clone()).clone();
        messages.push(Message {
            id,
            thread,
            // The calling agent, not bd's echo: create() always passes --actor.
            from: agent.to_string(),
            to: recipient.clone(),
            subject: Some(title.clone()),
            body: body.to_string(),
            created_at: issue.created_at,
            read: false,
            read_by: Vec::new(),
        });
        // One bead created, so one message sent. Counted here rather than after
        // the loop because a partial fan-out failure returns early, and the
        // recipients who *did* get it are exactly what a sender must not
        // re-send to (see `SendFailure::already_sent` above).
        otel::count(
            "pact.msg.sent",
            1,
            &attrs![
                "pact.msg.addressing" => addressing,
                "pact.msg.reply" => is_reply,
            ],
        );
    }

    // The about-<path> labels are already on every bead above: create_args
    // folds them into the same `create` call (pact-m7j.10.1), so there is no
    // second, separate tagging step left here to fail — and so no window in
    // which a bead exists but `about_path` cannot see what it is about yet.
    Ok(messages)
}

/// Unread messages tagged as being about `path`, for any recipient.
///
/// This is what makes `--to-owner-of` a delivery mechanism rather than an
/// address-book lookup. It deliberately ignores who a message was addressed to:
/// the point is that a message about `src/otel.rs` reaches whoever picks up
/// `src/otel.rs`, even — especially — when the agent it was addressed to has
/// exited. Reading it is the recipient's job; noticing it is the file's.
///
/// Filtered server-side via `--label=`, not fetched whole and filtered here
/// (pact-m7j.4.7): this used to call `list_issues` unfiltered and then keep
/// only the one label it wanted, so every message bead in the repo — every
/// other path's traffic, every thread, everything — was fetched (and, on br,
/// individually `show`n) just to throw almost all of it away. Both backends
/// bound `list --json` to one label directly; see `list_issues`.
pub fn about_path(cli: &BeadsCli, repo_root: &Path, path: &str) -> Result<Vec<Message>> {
    let label = format!("{ABOUT}{}", encode_path(path));
    let issues = list_issues(cli, repo_root, None, Some(&label))?;
    Ok(to_messages(issues, None))
}

/// FNV-1a, 64-bit: a fixed-seed, non-cryptographic hash, deliberately NOT
/// `std::collections::hash_map::DefaultHasher` — that one's `RandomState` seed
/// is randomized per PROCESS, so the same content would hash differently on
/// the retry than it did on the original send, defeating the entire point of
/// [`idempotency_key`]. FNV-1a needs no dependency for a few lines of stable,
/// deterministic mixing, and cryptographic strength buys nothing here: the
/// risk this key accepts is two DELIBERATELY identical messages colliding
/// (see `idempotency_key`'s own doc comment), not an adversary finding one.
fn fnv1a64(parts: &[&str]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // A separator between parts, so ("ab", "c") and ("a", "bc") — which
        // would otherwise concatenate to the same byte stream — hash
        // differently.
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// A deterministic id for bd's `--id`/`--force` upsert, so a retried `create`
/// after an ambiguous outcome (process killed after bd committed but before
/// returning, a hung subprocess, a harness that dropped stdout — all three
/// hit in production, see pact-m7j.6.4) lands on the SAME bead instead of a
/// second, near-identical one. `sent()`'s own doc comment tells a sender that
/// cannot confirm a send to re-send it; without this, that documented policy
/// is what mints the duplicate.
///
/// Deliberately a pure function of the send's own arguments — no counter, no
/// nonce, nothing written to `.pact/` — because pact's messaging layer keeps
/// zero local state by design and this key must not become the first
/// exception. That purity is also the accepted trade-off: two SEPARATE,
/// genuinely identical sends (same agent, same recipient, same thread
/// context, byte-identical title and body) collide into one bead rather than
/// two, same as a real retry would. For the routine, mostly-unique text real
/// messages carry, that is a good bargain against a duplicate that already
/// happened in production; it is not a content-addressed store's collision
/// resistance, and was not asked to be.
///
/// bd-only: `br` has no equivalent primitive on `create` at all (confirmed
/// against `br --help` and a real create-twice run — no `--id`, no
/// `--dedupe`; the only lever, a slug, still gets a random uniquifying
/// suffix every call), so a `br` retry is unprotected until `br` grows one.
///
/// Root messages only, even on bd: see [`create_args`] for why a reply
/// cannot carry this key alongside `--parent`.
fn idempotency_key(agent: &str, to: &str, parent: Option<&str>, title: &str, body: &str) -> String {
    let hash = fnv1a64(&[agent, to, parent.unwrap_or(""), title, body]);
    format!("pact-msg-{hash:016x}")
}

/// `create` args for one message bead. Owned Strings because they are all
/// interpolated; see the module docs for why `--no-inherit-labels` is not
/// optional on bd — and why br neither accepts nor needs it.
///
/// `about` is passed through to `--labels` so a bead is born already tagged
/// with the paths it is about (pact-m7j.10.1): both bd and br accept `-l` /
/// `--labels` at create time, and doing it here — rather than a second
/// `label add` after the bead exists — closes the window in which the bead is
/// visible to `about_path` without its label. `--no-inherit-labels` (bd-only,
/// above) does not fight this: it only suppresses labels inherited from
/// `--parent`, and an explicit `--labels` list is unaffected by it either way
/// — confirmed against a real bd store with both flags on the same call.
fn create_args(
    is_br: bool,
    to: &str,
    parent: Option<&str>,
    agent: &str,
    title: &str,
    body: &str,
    about: &[String],
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        "--type=message".to_string(),
        "--json".to_string(),
    ];
    if !is_br {
        // A child must not inherit the parent's read-by-* labels, or a reply
        // would be born already "read" by whoever read the message above it.
        // br rejects the flag outright and does not inherit labels anyway.
        args.push("--no-inherit-labels".to_string());
        // `--force` bypasses bd's project-prefix guard (confirmed against
        // `bd create --help` and a real scratch run). Needed unconditionally,
        // not just alongside `--id`: bd auto-derives a REPLY's id from its
        // parent (`<parent-id>.1`, `.2`, ...), and once the root carries our
        // synthetic `pact-msg-` id, every child's auto-derived id fails the
        // same prefix check even though the reply itself never passes `--id`
        // — confirmed by reproducing the exact "prefix mismatch" error on a
        // plain `--parent=pact-msg-...` create with no `--id` at all.
        args.push("--force".to_string());
        // Idempotency key: root messages only. `bd create` rejects `--id`
        // together with `--parent` outright ("cannot specify both --id and
        // --parent flags", confirmed against a real scratch run) — bd derives
        // a child's id from its parent, and an explicit id conflicts with
        // that. So a reply, or recipients 2..N of a fan-out, are unprotected
        // by this key; only the first create in a `send()` call (thread root,
        // no parent yet) gets one. Narrower than every create being safe to
        // retry, but it is the shape the reported incident actually was — a
        // single long send, not a reply — and reparenting after an id-only
        // create would double the subprocess calls on the common path
        // (every reply) to protect the uncommon one.
        if parent.is_none() {
            args.push(format!(
                "--id={}",
                idempotency_key(agent, to, parent, title, body)
            ));
        }
    }
    args.extend([
        format!("--title={title}"),
        format!("--description={body}"),
        format!("--assignee={to}"),
        // Records who (in pact's own identity scheme) sent this, in the
        // backend's audit trail — what `from` and `sent()` read back.
        format!("--actor={agent}"),
    ]);
    if let Some(p) = parent {
        args.push(format!("--parent={p}"));
    }
    if !about.is_empty() {
        let labels: Vec<String> = about
            .iter()
            .map(|p| format!("{ABOUT}{}", encode_path(p)))
            .collect();
        args.push(format!("--labels={}", labels.join(",")));
    }
    args
}

// One more argument than clippy's default likes, all of them genuinely
// distinct pieces of one bead (who it is to/from/about and what it says) —
// bundling them into a struct here would trade a lint suppression for a type
// with exactly one caller.
#[allow(clippy::too_many_arguments)]
fn create(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    to: &str,
    parent: Option<&str>,
    title: &str,
    body: &str,
    about: &[String],
) -> Result<BdIssue> {
    let args = create_args(cli.is_br(), to, parent, agent, title, body, about);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let stdout = cli.run(repo_root, &borrowed)?;
    serde_json::from_str(&stdout)
        .with_context(|| format!("parsing `{} create --json` output", cli.binary()))
}

pub fn inbox(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    unread_only: bool,
) -> Result<Vec<Message>> {
    let issues = list_issues(cli, repo_root, Some(agent), None)?;
    let mut messages = to_messages(issues, Some(agent));

    // Before the filter, so `--unread-only` and a plain listing report the same
    // queue depth. This is the observation pact-aw7.4 exists for: nobody can
    // see a mailbox rotting from inside the process that is not reading it.
    record_unread(&messages, Utc::now());

    if unread_only {
        messages.retain(|m| !m.read);
    }
    Ok(messages)
}

/// The exact argv [`list_issues`] hands to the backend, factored out so the
/// filters it decides to send are unit-testable without a real subprocess
/// (pact-m7j.4.7) — the same reasoning [`create_args`] is split out for.
fn list_args(is_br: bool, assignee: Option<&str>, label: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = if is_br {
        vec!["list".into(), "--json".into(), "--type=message".into()]
    } else {
        vec!["list".into(), "--include-infra".into(), "--json".into()]
    };
    if let Some(a) = assignee {
        args.push(format!("--assignee={a}"));
    }
    if let Some(l) = label {
        args.push(format!("--label={l}"));
    }
    args
}

/// Every message bead the backend will admit to, optionally narrowed to one
/// assignee and/or one label. The one place the two backends' listing argv
/// differ.
///
/// `label` is passed straight through as `--label=`, not applied after the
/// fact (pact-m7j.4.7): `about_path` used to fetch every message bead here and
/// filter client-side for the one label it wanted, paying for every OTHER
/// path's traffic too. Both bd 1.1.2 and br 0.2.19 support `-l/--label` as an
/// exact-match filter (`bd list --help`: "Filter by labels (AND: must have
/// ALL)"; `br list --help`: "Filter by label (AND logic..)"), so bounding the
/// query there bounds the response, and — on br — bounds the id set the `show`
/// fan-out below walks.
///
/// br's `list --json` omits `parent` even when `--label` narrows the result to
/// one bead — checked directly: a labelled reply's `list --json` entry still
/// carries no `parent`, only `dependency_count`. So the id-then-show fan-out
/// below still runs on br regardless of `label`; skipping it would silently
/// drop thread linkage for exactly the messages this filter exists to find.
/// (This corrects an assumption in the original bead that a label-filtered
/// `br list --json` already returns full record data — it does not, for
/// `parent` specifically.) `sent` and the TUI still call this with
/// `label: None`, unaffected.
fn list_issues(
    cli: &BeadsCli,
    repo_root: &Path,
    assignee: Option<&str>,
    label: Option<&str>,
) -> Result<Vec<BdIssue>> {
    let args = list_args(cli.is_br(), assignee, label);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let issues = parse_issues(&cli.run(repo_root, &borrowed)?, cli)?;
    if !cli.is_br() {
        return Ok(issues);
    }
    let ids: Vec<&str> = issues.iter().map(|i| i.id.as_str()).collect();
    if ids.is_empty() {
        // `br show` with no ids is an error, and an empty inbox is not one.
        return Ok(Vec::new());
    }
    show_many(cli, repo_root, &ids)
}

/// `show <id>… --json`, which returns an array on both backends.
fn show_many(cli: &BeadsCli, repo_root: &Path, ids: &[&str]) -> Result<Vec<BdIssue>> {
    let mut args = vec!["show"];
    args.extend_from_slice(ids);
    args.push("--json");
    let out = cli.run(repo_root, &args)?;
    serde_json::from_str(&out)
        .with_context(|| format!("parsing `{} show --json` output", cli.binary()))
}

/// Messages this agent sent, newest first (pact-rnc.7). A sender that cannot
/// confirm a send re-sends it — this is how an agent checks instead of guessing
/// (notably after the broken-pipe bug, pact-rnc.26, exits non-zero on a send
/// that actually landed).
pub fn sent(cli: &BeadsCli, repo_root: &Path, agent: &str) -> Result<Vec<Message>> {
    let mut messages = all_messages(cli, repo_root)?;
    messages.retain(|m| m.from == agent);
    messages.reverse(); // all_messages is oldest-first
    Ok(messages)
}

/// One `show <id> --json`, which returns an array even for a single id.
fn show(cli: &BeadsCli, repo_root: &Path, id: &str) -> Result<BdIssue> {
    show_many(cli, repo_root, &[id])?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("message {id} not found"))
}

/// The root of the thread `id` belongs to. Every member of a thread must resolve
/// to the SAME thread id, on every surface (pact-rnc.4): `msg inbox` reports the
/// root, so `msg read` reporting the queried id meant two pact commands
/// disagreeing, and the id `msg read` prints is a recipient's only source.
fn thread_root(cli: &BeadsCli, repo_root: &Path, id: &str) -> Result<BdIssue> {
    let start = show(cli, repo_root, id)?;
    Ok(walk_to_root(start, |parent| {
        show(cli, repo_root, parent).ok()
    }))
}

/// The `parent` walk itself, with the fetch injected so it is testable without a
/// `bd` on PATH. Stops at a bead with no parent, at a parent bd cannot produce,
/// after [`MAX_THREAD_DEPTH`] hops, and at a *non-message* parent — hanging a
/// message off a real issue (`--thread pact-rnc.4`) is deliberate, and that
/// issue is not itself part of the conversation.
fn walk_to_root(start: BdIssue, mut fetch: impl FnMut(&str) -> Option<BdIssue>) -> BdIssue {
    let mut issue = start;
    for _ in 0..MAX_THREAD_DEPTH {
        let Some(parent_id) = issue.parent.clone() else {
            break;
        };
        let Some(parent) = fetch(&parent_id) else {
            break;
        };
        if parent.issue_type != "message" {
            break;
        }
        issue = parent;
    }
    issue
}

/// Every message of the thread `id` belongs to, oldest first, and the root's id.
///
/// `id` may be any member of the thread, not just its root: a non-first recipient
/// of a fan-out send only ever sees her own child id, and reading it must give
/// her the whole conversation — and the root's id as the thread — rather than a
/// one-message "thread" whose id produces invisible grandchild replies.
///
/// Split out of [`read_thread`] so that [`peek_thread`] can answer with the same
/// records and no read-marking. The two must not each grow their own half-right
/// idea of what a thread contains: everything below the split is the *labelling*,
/// which is the only part an observer must not do.
fn gather_thread(cli: &BeadsCli, repo_root: &Path, id: &str) -> Result<(String, Vec<BdIssue>)> {
    let root = thread_root(cli, repo_root, id)?;
    let root_id = root.id.clone();

    let replies = replies_of(cli, repo_root, &root)?;

    let mut all = vec![root];
    all.extend(replies.into_iter().filter(|i| i.issue_type == "message"));
    // `msg read <id>` must always show <id>. It is normally the root or one of
    // its direct children and already here; a message parented on a *non-root*
    // member is not (older pact could create those, and bd data can be edited by
    // hand), and silently omitting the message the caller asked for is worse than
    // showing it alongside the thread. One extra `bd show` in that rare case only.
    if !all.iter().any(|i| i.id == id) {
        if let Ok(requested) = show(cli, repo_root, id) {
            all.push(requested);
        }
    }
    all.sort_by_key(|i| parse_ts(&i.created_at));
    Ok((root_id, all))
}

/// The thread `id` belongs to, **without marking anything read** — the twin of
/// [`read_thread`], in the same spirit as [`crate::lease::peek`] beside
/// `lease::list`.
///
/// This exists for the read-only MCP server (`pact mcp serve`), where answering
/// "what is in this thread" must not change delivery state. `read_thread` writes
/// a `read-by-<agent>` label, and that label is what a *sender* checks with `msg
/// sent` to decide whether a decision landed — so an observer who marked threads
/// read while looking at them would silently tell every sender their message had
/// been received by an agent that never saw it.
///
/// `viewer` only decides whose `read` flag is reported; passing `None` reports
/// the recipient's own.
///
/// Gated on the feature that uses it, or the default build warns it dead. That
/// is not a formality — `mark_read_by_id` shipped ungated, went red in CI on the
/// default build only, and was missed locally because `mise run check` was
/// running with `--features ui` and nothing else. `lint` compiles every feature
/// set now, which is what caught this one before it left the machine.
#[cfg(feature = "mcp")]
pub fn peek_thread(
    cli: &BeadsCli,
    repo_root: &Path,
    viewer: Option<&str>,
    id: &str,
) -> Result<Vec<Message>> {
    let (root_id, all) = gather_thread(cli, repo_root, id)?;
    Ok(all
        .into_iter()
        .map(|i| i.into_message(Some(&root_id), viewer))
        .collect())
}

/// The root message plus its direct replies, oldest first. Marks everything
/// shown as read for `agent`.
pub fn read_thread(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    id: &str,
) -> Result<Vec<Message>> {
    let (root_id, all) = gather_thread(cli, repo_root, id)?;

    // Bookkeeping must not destroy the thread the caller came for: if the
    // label write loses a race with another agent's bd write, warn and show the
    // messages anyway (they stay unread, so the next read retries). Same
    // reasoning as pact-rnc.26 — never fail work that already succeeded.
    let marked = match mark_read(cli, repo_root, agent, &all) {
        Ok(()) => true,
        Err(e) => {
            output::warn(&format!(
                "warning: could not mark thread {root_id} read: {e:#}"
            ));
            false
        }
    };

    let now = Utc::now();
    Ok(all
        .into_iter()
        .map(|i| {
            let mut m = i.into_message(Some(&root_id), Some(agent));
            // Just labelled, so the pre-read snapshot above doesn't show it yet.
            if marked && !m.read {
                m.read = true;
                m.read_by.push(agent.to_string());
                // This branch *is* "first read by this agent" — re-reading a
                // thread takes the other one — so it is the event to count, and
                // the message's age here is how long the sender waited.
                otel::count("pact.msg.read", 1, &attrs![]);
                if let Some(ms) = age_ms(&m.created_at, now) {
                    otel::record_ms("pact.msg.read_latency", ms, &attrs![]);
                }
            }
            m
        })
        .collect())
}

/// The direct replies to `root`, unfiltered by type.
///
/// bd answers this with a fresh `list --parent=<id>` every time. br has no such
/// filter; this used to read `show --json`'s own `dependents` field instead —
/// but that field is a snapshot from whenever `root` was fetched, so a reply
/// created after that fetch stayed invisible until something re-fetched the
/// root (pact-m7j.6.1). `br dep list <root> --direction up --json` is br's own
/// fresh query for exactly this edge, so it is asked here every time, the same
/// way the bd branch already asks `list --parent=` every time.
fn replies_of(cli: &BeadsCli, repo_root: &Path, root: &BdIssue) -> Result<Vec<BdIssue>> {
    if !cli.is_br() {
        let parent_arg = format!("--parent={}", root.id);
        let out = cli.run(
            repo_root,
            &["list", "--include-infra", "--json", &parent_arg],
        )?;
        return parse_issues(&out, cli);
    }
    let out = cli.run(
        repo_root,
        &["dep", "list", &root.id, "--direction", "up", "--json"],
    )?;
    let children = parse_child_ids(&out, cli)?;
    if children.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<&str> = children.iter().map(String::as_str).collect();
    show_many(cli, repo_root, &ids)
}

/// `dep list --direction up --json` -> the ids of `parent-child` edges only, in
/// the order br listed them. Any other edge type (`blocks`, `related`) is a
/// real dependency, not a reply, and must not be dragged into a conversation.
fn parse_child_ids(stdout: &str, cli: &BeadsCli) -> Result<Vec<String>> {
    let items: Vec<DepListItem> = serde_json::from_str(stdout)
        .with_context(|| format!("parsing `{} dep list --json` output", cli.binary()))?;
    Ok(items
        .into_iter()
        .filter(|d| d.dependency_type == "parent-child")
        .map(|d| d.issue_id)
        .collect())
}

/// The one place the `read-by-` label is spelled for writing; `into_message`
/// is the one place it is spelled for reading.
fn read_label(agent: &str) -> String {
    format!("{READ_BY}{agent}")
}

/// `bd label add <id>... read-by-<agent>` — one call for the whole thread, and
/// idempotent, so re-reading a thread is a no-op rather than a duplicate.
/// Mark one message read by id, for a caller that has the id but not the bead.
///
/// `pact ui` needs this: the dashboard is the human's inbox, and until it could
/// record a read the sender's `pact msg sent` said "unread" forever (pact-4tj).
/// `pact ui` is the only caller, so this is dead code in a default build —
/// gated rather than `allow`ed, because an `allow` would also hide the day it
/// stops being called at all.
#[cfg(feature = "ui")]
pub fn mark_read_by_id(cli: &BeadsCli, repo_root: &Path, agent: &str, id: &str) -> Result<()> {
    let label = read_label(agent);
    let actor = actor_arg(agent);
    cli.run(repo_root, &["label", "add", id, &label, &actor])
        .map(|_| ())
}

fn mark_read(cli: &BeadsCli, repo_root: &Path, agent: &str, issues: &[BdIssue]) -> Result<()> {
    let label = read_label(agent);
    let actor = actor_arg(agent);
    let mut args = vec!["label", "add"];
    args.extend(issues.iter().map(|i| i.id.as_str()));
    args.push(&label);
    args.push(&actor);
    cli.run(repo_root, &args).map(|_| ())
}

/// `--actor=<agent>`, so a backend write is attributed to the agent that caused
/// it rather than to whoever owns the checkout.
///
/// Without it every bead mutation from every agent in a fleet is recorded as the
/// human's `git user.name`, which makes the audit trail useless for exactly the
/// question it exists to answer: who did this. `send` has always passed it — this
/// is for the calls that did not.
///
/// Both backends accept the same flag, verified rather than assumed:
///
/// - `bd` 1.1.2 — `--actor string`, documented precedence `--actor` >
///   `$BEADS_ACTOR` > `git user.name` > `$USER`.
/// - `br` 0.2.19 — `--actor <ACTOR>`, alongside a richer per-agent scheme
///   (`BR_AGENT_NAME`, `BR_HARNESS`, `BR_MODEL`) that pact does not use, because
///   one flag that works on both beats two mechanisms to keep in step.
///
/// Deliberately NOT done by setting `git config user.name`: that would mutate a
/// checkout other agents share to fake attribution for one of them, which is the
/// opposite of an audit trail.
fn actor_arg(agent: &str) -> String {
    format!("--actor={agent}")
}

/// Every message bead in the repo, regardless of recipient, oldest first.
///
/// `bd list` hides message beads unless `--include-infra` is passed and has no
/// `--type` filter, so `issue_type == "message"` is filtered client-side; br
/// has the filter but no `--include-infra`, and the client-side pass is kept for
/// both so one backend cannot quietly leak non-messages the other rejects.
/// There is no querying agent here, so `read` is resolved against each
/// message's own recipient.
pub fn all_messages(cli: &BeadsCli, repo_root: &Path) -> Result<Vec<Message>> {
    Ok(to_messages(list_issues(cli, repo_root, None, None)?, None))
}

/// `list --json` output -> issues. bd emits a bare array, br an envelope.
fn parse_issues(stdout: &str, cli: &BeadsCli) -> Result<Vec<BdIssue>> {
    let payload: ListPayload = serde_json::from_str(stdout)
        .with_context(|| format!("parsing `{} list --json` output", cli.binary()))?;
    Ok(payload.into_issues())
}

/// Issues -> message beads only, oldest first.
fn to_messages(issues: Vec<BdIssue>, viewer: Option<&str>) -> Vec<Message> {
    let mut messages: Vec<Message> = issues
        .into_iter()
        .filter(|i| i.issue_type == "message")
        .map(|i| i.into_message(None, viewer))
        .collect();
    messages.sort_by(oldest_first);
    messages
}

/// pact-rnc.20: compare parsed instants, never the raw strings. Two writers
/// reach these lists — bd's `Z` and pact's own chrono `+00:00` — and `'+'`
/// (0x2B) sorts before `'Z'` (0x5A), so a string compare calls an older `Z`
/// stamp newer than a `+00:00` one. Unparsable sorts oldest (None < Some)
/// rather than blowing up, same as `agents::parse_ts`.
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

    fn bd() -> BeadsCli {
        BeadsCli { binary: "bd" }
    }

    fn br() -> BeadsCli {
        BeadsCli { binary: "br" }
    }

    /// The pre-br entry point, kept for the tests that predate the split so
    /// they still assert the same thing about the same bytes.
    fn parse_messages(stdout: &str, viewer: Option<&str>) -> Result<Vec<Message>> {
        Ok(to_messages(parse_issues(stdout, &bd())?, viewer))
    }

    /// A real `br`-initialised repo, for the one test (pact-m7j.6.1) that needs
    /// actual replies fetched over a live subprocess rather than a JSON
    /// fixture — `replies_of` and `thread_root` are private, so `tests/cli.rs`
    /// (which only sees the compiled binary) cannot reach them directly.
    /// Mirrors `tests/cli.rs`'s `beads_repo`: skip with a reason on stderr
    /// rather than failing the whole file when `br` is not on PATH.
    fn br_test_workspace(test: &str) -> Option<tempfile::TempDir> {
        let on_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("br").is_file()))
            .unwrap_or(false);
        if !on_path {
            eprintln!("SKIP {test}: br not found on PATH");
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let setup: [&[&str]; 4] = [
            &["git", "init", "-q", "."],
            &["git", "config", "user.email", "tests@pact.invalid"],
            &["git", "config", "user.name", "pact tests"],
            &["br", "init"],
        ];
        for cmd in setup {
            match std::process::Command::new(cmd[0])
                .args(&cmd[1..])
                .current_dir(tmp.path())
                .output()
            {
                Ok(o) if o.status.success() => {}
                _ => {
                    eprintln!("SKIP {test}: `{}` failed", cmd.join(" "));
                    return None;
                }
            }
        }
        Some(tmp)
    }

    /// pact-m7j.6.1: `replies_of`'s br branch used to answer from `root`'s own
    /// `dependents` field — a snapshot from whenever `root` was fetched — so a
    /// reply created after that fetch stayed invisible to any caller still
    /// holding the old `root` value. `gather_thread` itself never reuses a
    /// `root` across two `replies_of` calls (it re-fetches at the top of every
    /// call, which is why this cannot be reproduced by calling `msg read`
    /// twice from the shell — confirmed against real `br` 0.2.19), but nothing
    /// stopped a caller that fetches `root` once from asking twice, and that is
    /// exactly the shape this test drives directly against the two private
    /// functions involved.
    ///
    /// Causality: with the old code (`child_ids(root)` reading `root.dependents`)
    /// the second call below returns 1, matching what existed when `root` was
    /// fetched; with the fix (a fresh `dep list` every call) it returns 2.
    #[test]
    fn br_replies_are_queried_fresh_not_read_from_a_stale_root_snapshot() {
        let Some(tmp) =
            br_test_workspace("br_replies_are_queried_fresh_not_read_from_a_stale_root_snapshot")
        else {
            return;
        };
        let cli = br();
        let root = create(
            &cli,
            tmp.path(),
            "alpha",
            "bravo",
            None,
            "root",
            "root body",
            &[],
        )
        .expect("create root");
        create(
            &cli,
            tmp.path(),
            "bravo",
            "alpha",
            Some(&root.id),
            "root",
            "reply one",
            &[],
        )
        .expect("create reply1");

        // Fetched BEFORE reply2 exists — this is the "earlier show call"
        // snapshot the bug held onto.
        let stale_root = thread_root(&cli, tmp.path(), &root.id).expect("thread_root");
        assert_eq!(
            replies_of(&cli, tmp.path(), &stale_root).unwrap().len(),
            1,
            "only reply1 exists so far"
        );

        create(
            &cli,
            tmp.path(),
            "bravo",
            "alpha",
            Some(&root.id),
            "root",
            "reply two",
            &[],
        )
        .expect("create reply2");

        // Same `stale_root` value, reused rather than re-fetched.
        let replies = replies_of(&cli, tmp.path(), &stale_root).expect("replies_of");
        assert_eq!(
            replies.len(),
            2,
            "a reply created after root was fetched must still show up: {replies:?}"
        );
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

    /// Shape copied from real `bd list --include-infra --json` output, labels
    /// included (bd hydrates them by default and emits `null` when there are
    /// none).
    const LIST_JSON: &str = r#"[
      {"id":"pact-wisp-1","title":"hi","description":"body one",
       "assignee":"msg-fix","created_by":"tui-dev","created_at":"2026-07-31T07:20:00Z",
       "issue_type":"message","labels":["read-by-msg-fix","urgent"]},
      {"id":"pact-wisp-2","title":"re: hi","description":"body two",
       "assignee":"msg-fix","created_by":"Clement HUSSENOT-DESENONGES",
       "created_at":"2026-07-31T07:10:00Z","issue_type":"message","parent":"pact-wisp-1",
       "labels":null},
      {"id":"pact-wisp-3","title":"anon","assignee":"msg-fix",
       "created_at":"2026-07-31T07:30:00Z","issue_type":"message",
       "labels":["read-by-someone-else"]},
      {"id":"pact-rnc.1","title":"a real bug, not a message",
       "created_at":"2026-07-31T07:00:00Z","issue_type":"bug","created_by":"someone"}
    ]"#;

    #[test]
    fn parse_messages_keeps_from_and_drops_non_messages() {
        let msgs = parse_messages(LIST_JSON, None).unwrap();

        // "bug" filtered out client-side; oldest first.
        let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["pact-wisp-2", "pact-wisp-1", "pact-wisp-3"]);

        // from survives the round trip, verbatim -- a pact agent name for
        // --actor sends, a git user name otherwise, "" when bd reports none.
        let from: Vec<&str> = msgs.iter().map(|m| m.from.as_str()).collect();
        assert_eq!(
            from,
            ["Clement HUSSENOT-DESENONGES", "tui-dev", ""],
            "missing created_by must yield \"\", not a panic"
        );

        // Reply is pinned to its parent thread; roots thread on themselves.
        assert_eq!(msgs[0].thread, "pact-wisp-1");
        assert_eq!(msgs[1].thread, "pact-wisp-1");
        assert_eq!(msgs[2].thread, "pact-wisp-3");
        assert!(msgs.iter().all(|m| m.to == "msg-fix"));
        assert_eq!(
            msgs[2].body, "",
            "missing description is empty, not a panic"
        );
    }

    /// pact-rnc.17: read state is bd labels now, so a sender can see it too.
    #[test]
    fn read_by_comes_from_labels_and_read_follows_the_viewer() {
        let msgs = parse_messages(LIST_JSON, None).unwrap();
        // Only read-by-* labels land in read_by; "urgent" is not a reader.
        assert_eq!(msgs[1].read_by, ["msg-fix"]);
        assert_eq!(msgs[2].read_by, ["someone-else"]);
        assert!(msgs[0].read_by.is_empty(), "labels:null is not a panic");

        // No viewer (all_messages): read is resolved against the recipient.
        let read: Vec<bool> = msgs.iter().map(|m| m.read).collect();
        assert_eq!(read, [false, true, false]);

        // Viewer = the querying agent, whoever the recipient happens to be.
        let mine: Vec<bool> = parse_messages(LIST_JSON, Some("msg-fix"))
            .unwrap()
            .iter()
            .map(|m| m.read)
            .collect();
        assert_eq!(mine, [false, true, false]);
        let theirs: Vec<bool> = parse_messages(LIST_JSON, Some("someone-else"))
            .unwrap()
            .iter()
            .map(|m| m.read)
            .collect();
        assert_eq!(theirs, [false, false, true]);
    }

    /// What `msg inbox --unread-only` and the TUI's unread badge both do.
    #[test]
    fn unread_only_filtering_still_works_off_labels() {
        let mut msgs = parse_messages(LIST_JSON, Some("msg-fix")).unwrap();
        msgs.retain(|m| !m.read);
        let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["pact-wisp-2", "pact-wisp-3"]);
        assert_eq!(msgs.len(), 2, "the unread badge counts these");
    }

    /// pact-rnc.20: String::cmp gets both of these backwards.
    #[test]
    fn sorting_mixes_bd_z_stamps_with_pact_offset_stamps() {
        const MIXED: &str = r#"[
          {"id":"pact-0720","title":"pact, 07:20Z written as +02:00","assignee":"a",
           "created_at":"2026-07-31T09:20:00+02:00","issue_type":"message"},
          {"id":"bd-0800","title":"bd, 08:00Z","assignee":"a",
           "created_at":"2026-07-31T08:00:00Z","issue_type":"message"},
          {"id":"bd-0900","title":"bd, 09:00Z","assignee":"a",
           "created_at":"2026-07-31T09:00:00Z","issue_type":"message"},
          {"id":"pact-0900","title":"pact, the same instant as bd-0900","assignee":"a",
           "created_at":"2026-07-31T09:00:00+00:00","issue_type":"message"}
        ]"#;
        // The two bytes that mislead a string compare: '+' (0x2B) sorts before
        // 'Z' (0x5A), so the same instant from pact looks older than from bd...
        assert!("2026-07-31T09:00:00+00:00" < "2026-07-31T09:00:00Z");
        // ...and a local-offset stamp's digits swamp the offset entirely.
        assert!("2026-07-31T09:20:00+02:00" > "2026-07-31T08:00:00Z");

        let ids: Vec<String> = parse_messages(MIXED, None)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            ids,
            ["pact-0720", "bd-0800", "bd-0900", "pact-0900"],
            "07:20Z < 08:00Z < 09:00Z, and the 09:00 tie keeps input order; \
             a string sort would say bd-0800, pact-0900, bd-0900, pact-0720"
        );
    }

    /// pact-rnc.7: an outbox is a filter on `from`, newest first.
    #[test]
    fn sent_is_only_this_agents_sends_newest_first() {
        // Same body as sent(), which cannot run here (it shells out to bd).
        let mut msgs = parse_messages(LIST_JSON, None).unwrap();
        msgs.retain(|m| m.from == "tui-dev");
        msgs.reverse();
        assert_eq!(
            msgs.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["pact-wisp-1"],
            "other agents' sends, and the human's, are not mine"
        );

        let mut human = parse_messages(LIST_JSON, None).unwrap();
        human.retain(|m| m.from == "Clement HUSSENOT-DESENONGES");
        assert_eq!(human.len(), 1);

        // Newest first, unlike every other listing here.
        let mut all = parse_messages(LIST_JSON, None).unwrap();
        all.reverse();
        assert_eq!(all[0].id, "pact-wisp-3");
    }

    /// pact-rnc.4: recipients 2..N are children of the first one's bead, which
    /// is what makes them one thread `read_thread` can return whole.
    #[test]
    fn multi_recipient_send_parents_the_rest_on_the_root() {
        let root = create_args(
            false,
            "mascot-dev",
            None,
            "animator",
            "Alarmed loops",
            "body",
            &[],
        );
        assert!(
            !root.iter().any(|a| a.starts_with("--parent=")),
            "a new thread's root has no parent: {root:?}"
        );
        assert!(root.contains(&"--assignee=mascot-dev".to_string()));
        assert!(root.contains(&"--actor=animator".to_string()));

        // Second recipient, parented on the root bead the first create returned.
        let child = create_args(
            false,
            "tui-dev",
            Some("pact-wisp-a2u"),
            "animator",
            "Alarmed loops",
            "body",
            &[],
        );
        assert!(child.contains(&"--parent=pact-wisp-a2u".to_string()));
        assert!(child.contains(&"--assignee=tui-dev".to_string()));
        // Same subject and body, one thread — not N near-duplicate threads.
        assert!(child.contains(&"--title=Alarmed loops".to_string()));

        // Without this, a child inherits the parent's read-by-* labels and is
        // born "already read" (verified against bd 1.1.0).
        for args in [&root, &child] {
            assert!(args.contains(&"--no-inherit-labels".to_string()));
        }
    }

    /// pact-m7j.6.4: a retried `create` with identical arguments must produce
    /// the identical `--id`, or the whole point of the upsert is lost.
    #[test]
    fn idempotency_key_is_stable_across_repeated_calls() {
        let a = idempotency_key("animator", "mascot-dev", None, "Alarmed loops", "body");
        let b = idempotency_key("animator", "mascot-dev", None, "Alarmed loops", "body");
        assert_eq!(a, b);
        assert!(a.starts_with("pact-msg-"));
    }

    /// Anything that changes what the message actually says or who it is
    /// between must not collide — that would silently merge two distinct
    /// messages into one bead, which is the one thing this key must not do.
    #[test]
    fn idempotency_key_differs_when_any_input_differs() {
        let base = idempotency_key("animator", "mascot-dev", None, "subject", "body");
        let variants = [
            idempotency_key("tui-dev", "mascot-dev", None, "subject", "body"),
            idempotency_key("animator", "docs-writer", None, "subject", "body"),
            idempotency_key(
                "animator",
                "mascot-dev",
                Some("pact-wisp-a2u"),
                "subject",
                "body",
            ),
            idempotency_key("animator", "mascot-dev", None, "other subject", "body"),
            idempotency_key("animator", "mascot-dev", None, "subject", "other body"),
            // Concatenation ambiguity: "sub" + "ject" must not equal "subj" + "ect".
            idempotency_key("animator", "mascot-dev", None, "subj", "ectbody"),
        ];
        for v in variants {
            assert_ne!(base, v, "distinct inputs collided: {base} == {v}");
        }
    }

    /// `create_args` wires the key through with `--force`, and only for bd —
    /// br has no equivalent primitive (module docs) and would reject
    /// `--no-inherit-labels`-adjacent bd-only flags outright.
    #[test]
    fn create_args_passes_a_deterministic_id_and_force_on_bd_only() {
        let bd_args = create_args(
            false,
            "mascot-dev",
            None,
            "animator",
            "subject",
            "body",
            &[],
        );
        let expected_id = format!(
            "--id={}",
            idempotency_key("animator", "mascot-dev", None, "subject", "body")
        );
        assert!(
            bd_args.contains(&expected_id),
            "expected {expected_id} in {bd_args:?}"
        );
        assert!(bd_args.contains(&"--force".to_string()));

        let br_args = create_args(true, "mascot-dev", None, "animator", "subject", "body", &[]);
        assert!(
            !br_args
                .iter()
                .any(|a| a.starts_with("--id=") || a == "--force"),
            "br has no --id/--force primitive: {br_args:?}"
        );
    }

    /// pact-m7j.4.7: `about_path` used to fetch every message bead unfiltered
    /// and filter client-side for the one label it wanted. The label must
    /// reach the backend's own argv — on both backends, since both support
    /// `--label` — not just pact's post-filter.
    #[test]
    fn list_args_passes_the_label_filter_through_to_both_backends() {
        let label = "about-src__foo.rs";
        for is_br in [false, true] {
            let args = list_args(is_br, None, Some(label));
            assert!(
                args.contains(&format!("--label={label}")),
                "backend (is_br={is_br}) must receive the label filter server-side: {args:?}"
            );
        }
    }

    /// `inbox` and `all_messages` call this with `label: None` and must not
    /// start silently narrowing their listing to one label.
    #[test]
    fn list_args_has_no_label_filter_when_none_is_given() {
        for is_br in [false, true] {
            let args = list_args(is_br, None, None);
            assert!(
                !args.iter().any(|a| a.starts_with("--label=")),
                "no label requested, none should be sent: {args:?}"
            );
        }
    }

    /// The bd branch needs `--include-infra` (message beads are otherwise
    /// hidden) and the br branch needs `--type=message` (bd has no such
    /// filter and does it client-side) — a label filter must not replace
    /// either.
    #[test]
    fn list_args_keeps_the_backend_specific_message_filter_alongside_a_label() {
        let bd_args = list_args(false, None, Some("about-x"));
        assert!(
            bd_args.contains(&"--include-infra".to_string()),
            "{bd_args:?}"
        );

        let br_args = list_args(true, None, Some("about-x"));
        assert!(
            br_args.contains(&"--type=message".to_string()),
            "{br_args:?}"
        );
    }

    /// `bd create` rejects `--id` together with `--parent` outright — a reply
    /// (or recipients 2..N of a fan-out) must not carry the idempotency key,
    /// or every reply in a thread would fail outright. `--force` is still
    /// needed: bd auto-derives a reply's id from its parent
    /// (`<parent-id>.1`), and once the parent carries our synthetic id, the
    /// derived child id fails bd's prefix check too — confirmed by
    /// reproducing the exact error against a real scratch store.
    #[test]
    fn create_args_omits_the_idempotency_key_but_keeps_force_when_there_is_a_parent() {
        let reply = create_args(
            false,
            "tui-dev",
            Some("pact-wisp-a2u"),
            "animator",
            "subject",
            "body",
            &[],
        );
        assert!(
            !reply.iter().any(|a| a.starts_with("--id=")),
            "a reply must not carry --id alongside --parent: {reply:?}"
        );
        assert!(
            reply.contains(&"--force".to_string()),
            "a reply still needs --force, since its auto-derived id inherits \
             the parent's prefix mismatch: {reply:?}"
        );
        assert!(reply.contains(&"--parent=pact-wisp-a2u".to_string()));
    }

    /// pact-m7j.10.1: the about-<path> labels must ride the SAME `create` call
    /// as the bead itself — one argv, one subprocess — so there is no window
    /// between "bead exists" and "bead is tagged" for a concurrent
    /// `about_path` read to land in. Before this fix, `send()` created the
    /// bead here and only tagged it in a second `label add` call after the
    /// whole recipient loop finished.
    #[test]
    fn create_args_folds_about_labels_into_the_same_create_call() {
        let about = vec!["src/msg.rs".to_string(), "src/main.rs".to_string()];
        let args = create_args(
            false,
            "mascot-dev",
            None,
            "animator",
            "subject",
            "body",
            &about,
        );
        assert!(
            args.contains(&"--labels=about-src__msg-rs,about-src__main-rs".to_string()),
            "expected a single --labels flag carrying both paths: {args:?}"
        );
        // No separate tagging call exists any more — create_args builds one
        // argv and that argv is everything `create()` runs.
        assert!(!args.iter().any(|a| a == "label" || a == "add"));

        // br takes the same flag as bd, unconditionally.
        let br_args = create_args(
            true,
            "mascot-dev",
            None,
            "animator",
            "subject",
            "body",
            &about,
        );
        assert!(
            br_args.contains(&"--labels=about-src__msg-rs,about-src__main-rs".to_string()),
            "{br_args:?}"
        );
    }

    /// The common case (no `--to-owner-of`) must not grow a `--labels` flag it
    /// has nothing to put in — an empty `--labels=` would be a label named "".
    #[test]
    fn create_args_omits_labels_flag_when_about_is_empty() {
        let args = create_args(
            false,
            "mascot-dev",
            None,
            "animator",
            "subject",
            "body",
            &[],
        );
        assert!(!args.iter().any(|a| a.starts_with("--labels=")), "{args:?}");
    }

    /// bd's `--no-inherit-labels` only suppresses labels inherited FROM THE
    /// PARENT — it must not also eat an explicit `--labels` list handed to the
    /// same `create` call, or a reply that is about a path would be tagged on
    /// bd's own root message but silently lose the tag on a reply. Verified
    /// empirically against a real bd 1.1.0 store (see [`encode_path`]'s doc
    /// comment for the br side of this verification).
    #[test]
    fn no_inherit_labels_and_an_explicit_labels_list_coexist_on_bd() {
        let reply = create_args(
            false,
            "tui-dev",
            Some("pact-wisp-a2u"),
            "animator",
            "subject",
            "body",
            &["src/x.rs".to_string()],
        );
        assert!(reply.contains(&"--no-inherit-labels".to_string()));
        assert!(reply.contains(&"--labels=about-src__x-rs".to_string()));
    }

    /// br rejects a label containing `.` outright ("invalid characters (only
    /// alphanumeric, hyphen, underscore, colon allowed)", exit 4 — confirmed
    /// against a real br 0.2.19 store). Every real file path has one, so this
    /// must not reach br's argv unescaped.
    #[test]
    fn encode_path_sanitizes_characters_br_labels_reject() {
        assert_eq!(encode_path("src/otel.rs"), "src__otel-rs");
        assert_eq!(encode_path("a/b/c"), "a__b__c");
        assert_eq!(encode_path("plain"), "plain");
        // The allowed set survives untouched.
        assert_eq!(encode_path("a-b_c:d"), "a-b_c:d");
    }

    fn issue(json: &str) -> BdIssue {
        serde_json::from_str(json).unwrap()
    }

    /// `into_message`'s `thread` argument overrides the bead's own `parent` — so
    /// whatever `read_thread` passes there IS the thread every row reports.
    /// Deliberately not named after the root: the caller is what has to resolve
    /// the root (see the `walk_to_root` tests), and a literal called
    /// `"pact-wisp-root"` here made this assertion look like it pinned the root
    /// when it only pinned the override, and it passed just as happily while
    /// `read_thread` was passing the queried child id (pact-rnc.4/22).
    #[test]
    fn into_message_thread_argument_overrides_the_beads_parent() {
        let m = issue(
            r#"{"id":"pact-wisp-9","title":"deep reply","assignee":"human",
                "created_by":"lease-fix","created_at":"2026-07-31T08:00:00Z",
                "issue_type":"message","parent":"pact-wisp-8",
                "labels":["read-by-human"]}"#,
        )
        .into_message(Some("whatever-read-thread-passes"), Some("human"));
        assert_eq!(m.thread, "whatever-read-thread-passes");
        assert_eq!(m.from, "lease-fix");
        assert_eq!(m.to, "human");
        assert!(m.read);
        assert_eq!(m.read_by, ["human"]);
    }

    /// pact-rnc.4: the walk that makes every thread member report the same
    /// thread id. Recipient 2 of a fan-out holds `...8g3.1`; the thread is
    /// `...8g3`, and a reply parented anywhere else is invisible to the thread.
    #[test]
    fn walk_to_root_climbs_to_the_thread_root() {
        let beads = |id: &str, parent: Option<&str>, kind: &str| {
            issue(&format!(
                r#"{{"id":"{id}","title":"t","assignee":"a",
                     "created_at":"2026-07-31T08:00:00Z","issue_type":"{kind}"
                     {}}}"#,
                parent
                    .map(|p| format!(r#","parent":"{p}""#))
                    .unwrap_or_default()
            ))
        };
        let chain = |id: &str| match id {
            "root" => Some(beads("root", None, "message")),
            "child" => Some(beads("child", Some("root"), "message")),
            "epic" => Some(beads("epic", None, "epic")),
            _ => None,
        };

        // Grandchild -> child -> root, which is the shape today's bug creates.
        let grandchild = beads("grandchild", Some("child"), "message");
        assert_eq!(walk_to_root(grandchild, chain).id, "root");
        // A root walks nowhere.
        assert_eq!(
            walk_to_root(beads("root", None, "message"), chain).id,
            "root"
        );
        // A parent bd cannot produce (deleted, or another repo) stops the walk
        // where it is, rather than failing the read.
        let orphan = beads("orphan", Some("vanished"), "message");
        assert_eq!(walk_to_root(orphan, chain).id, "orphan");
        // `--thread <issue-id>`: the issue is not part of the conversation.
        let on_an_issue = beads("note", Some("epic"), "message");
        assert_eq!(walk_to_root(on_an_issue, chain).id, "note");
    }

    /// Corrupt/hand-edited parents pointing at each other must terminate, not
    /// hang the CLI — the only reason `MAX_THREAD_DEPTH` exists.
    #[test]
    fn walk_to_root_gives_up_on_a_parent_cycle() {
        let cyclic = |id: &str| {
            let other = if id == "a" { "b" } else { "a" };
            Some(issue(&format!(
                r#"{{"id":"{id}","title":"t","assignee":"x","parent":"{other}",
                     "created_at":"2026-07-31T08:00:00Z","issue_type":"message"}}"#
            )))
        };
        let start = issue(
            r#"{"id":"a","title":"t","assignee":"x","parent":"b",
                "created_at":"2026-07-31T08:00:00Z","issue_type":"message"}"#,
        );
        let landed = walk_to_root(start, cyclic);
        assert!(landed.id == "a" || landed.id == "b", "{}", landed.id);
    }

    /// The label `mark_read` writes must be the label `into_message` reads.
    #[test]
    fn the_read_label_write_and_read_paths_agree() {
        let json = format!(
            r#"[{{"id":"m","title":"t","assignee":"msg-fix",
                  "created_at":"2026-07-31T07:00:00Z","issue_type":"message",
                  "labels":["{}"]}}]"#,
            read_label("msg-fix")
        );
        let msgs = parse_messages(&json, Some("msg-fix")).unwrap();
        assert!(msgs[0].read);
        assert_eq!(msgs[0].read_by, ["msg-fix"]);
    }

    /// pact-l94: br rejects `--no-inherit-labels` outright —
    /// `error: unexpected argument '--no-inherit-labels' found`, exit non-zero —
    /// so passing it would break every single send on that backend. Everything
    /// else in the create line is identical, verified against br 0.2.19.
    #[test]
    fn br_creates_the_same_bead_without_bds_label_inheritance_flag() {
        let brs = create_args(
            true,
            "docs-writer",
            Some("brlab-udp"),
            "br-dev",
            "subj",
            "b",
            &[],
        );
        assert!(
            !brs.contains(&"--no-inherit-labels".to_string()),
            "br errors on this flag: {brs:?}"
        );
        // Dropping it is safe rather than a silent loss of the guarantee: a br
        // child is born with no labels, so it cannot arrive pre-"read".
        for expected in [
            "create",
            "--type=message",
            "--json",
            "--title=subj",
            "--description=b",
            "--assignee=docs-writer",
            "--actor=br-dev",
            "--parent=brlab-udp",
        ] {
            assert!(brs.contains(&expected.to_string()), "missing {expected}");
        }

        // bd keeps the flag, plus its own `--id`/`--force` idempotency pair
        // (pact-m7j.6.4 — br has no equivalent primitive to give the same
        // one), and every OTHER argument is byte-identical, so the shipped
        // backend cannot regress on the way past.
        let bds = create_args(
            false,
            "docs-writer",
            Some("brlab-udp"),
            "br-dev",
            "subj",
            "b",
            &[],
        );
        let without: Vec<&String> = bds
            .iter()
            .filter(|a| *a != "--no-inherit-labels" && *a != "--force" && !a.starts_with("--id="))
            .collect();
        assert_eq!(without, brs.iter().collect::<Vec<_>>());
    }

    /// pact-l94: `bd list --json` is a bare array, `br list --json` an envelope.
    /// Both strings below are real output, trimmed to the fields pact reads.
    #[test]
    fn list_parsing_accepts_bds_bare_array_and_brs_envelope() {
        const BR_LIST: &str = r#"{"issues":[
          {"id":"brlab-udp","title":"hello","description":"body","status":"open",
           "issue_type":"message","assignee":"br-dev","created_by":"sender",
           "created_at":"2026-08-02T07:23:58.797517176Z",
           "labels":["read-by-br-dev"],"dependency_count":0,"dependent_count":0}
        ],"total":1,"limit":0,"offset":0,"has_more":false}"#;

        let from_br = to_messages(parse_issues(BR_LIST, &br()).unwrap(), Some("br-dev"));
        assert_eq!(from_br.len(), 1);
        assert_eq!(from_br[0].from, "sender");
        assert_eq!(from_br[0].to, "br-dev");
        assert!(from_br[0].read, "read-by- labels work the same on br");

        // The bd array still parses through the same function, unchanged.
        assert_eq!(parse_issues(LIST_JSON, &bd()).unwrap().len(), 4);
    }

    /// pact-l94/pact-m7j.6.1: br has no `list --parent`, so replies come from
    /// a fresh `dep list <root> --direction up --json` query — not from
    /// `show`'s own `dependents` snapshot (see `replies_of`'s doc comment for
    /// why). JSON copied from a real `br dep list --direction up --json`.
    #[test]
    fn br_dep_list_parses_parent_child_edges_and_ignores_the_rest() {
        const DEP_LIST_JSON: &str = r#"[
          {"issue_id":"brlab-udp.2","depends_on_id":"brlab-udp","type":"parent-child",
           "title":"r2","status":"open","priority":2},
          {"issue_id":"brlab-udp.1","depends_on_id":"brlab-udp","type":"parent-child",
           "title":"reply","status":"open","priority":2},
          {"issue_id":"brlab-zzz","depends_on_id":"brlab-udp","type":"blocks",
           "title":"a real blocker","status":"open","priority":2}
        ]"#;
        assert_eq!(
            parse_child_ids(DEP_LIST_JSON, &br()).unwrap(),
            ["brlab-udp.2", "brlab-udp.1"],
            "the blocks edge must not be dragged into the conversation"
        );

        // No replies at all: an empty array, not a parse failure.
        assert!(parse_child_ids("[]", &br()).unwrap().is_empty());
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

    /// pact-rnc.4: sending to nobody is a mistake, not a silent no-op. Bails
    /// before bd is ever spawned, hence the deliberately bogus binary.
    #[test]
    fn send_with_no_recipients_is_an_error() {
        let cli = BeadsCli {
            binary: "pact-definitely-not-bd",
        };
        let err = send(
            &cli,
            Path::new("/nonexistent"),
            "msg-fix",
            &[],
            Draft {
                thread: None,
                subject: None,
                body: "body",
                about: &[],
            },
        )
        .expect_err("empty --to must not be accepted");
        assert!(err.to_string().contains("no recipients"), "{err}");
    }
}
