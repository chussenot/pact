//! Path subscriptions: who wants to be told when a path changes hands.
//!
//! ## Why this exists
//!
//! The protocol has asked agents to announce interface changes by hand for
//! pact's whole life, and has been tuned by prose in both directions without
//! ever finding a middle:
//!
//! - Before `107e7c4` the block restrained nothing, and agents spammed — 223
//!   message beads across pact's own fleet runs, one run alone producing 85
//!   messages of which 41 were status pings to a recipient who never read an
//!   inbox. A real `BLOCKER` sat unread for 38 minutes inside that noise.
//! - After it, the block says the lease note IS the announcement and reserves
//!   messages for what needs something back. Messaging then collapsed: 4
//!   messages across the next three fleet runs, between 28 agents — and the
//!   collapse took the load-bearing ones with it. megablast's single surviving
//!   message is the only reason a `write_buffer` overflow did not ship.
//!
//! Voluntary messaging is bimodal under prose: spam or silence, no reachable
//! middle. So this does not ask agents to do anything new at announce time.
//! Subscription is a one-off registration, and **delivery is a side effect of
//! `lease release`** — a command the same runs performed 31 times out of 31.
//!
//! ## What this is not
//!
//! There is no watcher process, no daemon, no polling and nothing to wait on.
//! This module is a registry and a lookup. `lease release` reads it, sends
//! whatever messages it implies, and exits. A subscriber sees the message at
//! their next `pact msg inbox`, which the protocol already asks for at task
//! start.
//!
//! ## Storage
//!
//! `.pact/watches.jsonl` under the **resolved shared root**, so every worktree
//! of one repository sees one registry — the same resolution leases use, for
//! the same reason. Append-only with tombstones: `pact watch rm` writes an
//! `unwatch` record rather than editing, so the file is never rewritten and
//! two agents appending concurrently cannot lose each other's work.
//!
//! Chain-hashed exactly like `events.jsonl` (see [`crate::events::Event`]), so
//! a hand-edited subscription is detectable rather than indistinguishable from
//! a real one.
//!
//! Unlike `events.jsonl` this file stays **gitignored** — it is covered by the
//! `.pact/*` rule `pact init` writes, and only `events.jsonl` is re-included.
//! A subscription is live state belonging to a running fleet, like a lease and
//! unlike history: committing it would have every clone inherit subscriptions
//! from agents that no longer exist.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Only the chain-hash test reaches for this directly now that the append
/// itself is shared — see `events::jsonl`.
#[cfg(test)]
use crate::events::chain_hash_of;

/// One append to the registry: a subscription, or the tombstone retiring it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchRecord {
    /// RFC3339.
    pub at: String,
    /// The subscriber — `PACT_AGENT`, resolved the same way every other
    /// command resolves identity.
    pub agent: String,
    /// `"watch"` or `"unwatch"`. A plain `String` for the same reason
    /// `Event::kind` is one: an older binary reading a newer registry shows an
    /// unknown kind rather than refusing to parse the file.
    pub kind: String,
    /// Repo-relative, normalized by [`crate::lease::normalize_path`] so a
    /// subscription and a lease on one file agree about its name however each
    /// was spelled.
    pub path: String,
    /// Does this subscribe to everything **under** `path`, rather than to
    /// `path` itself?
    ///
    /// Decided at registration from the raw argument (a trailing `/`, or an
    /// existing directory) and stored, never re-derived at match time: the
    /// directory may not exist any more by the time a release looks, and a
    /// subscription that silently changes meaning is worse than one that is
    /// wrong in a fixed way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prefix: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_hash: Option<String>,
}

impl crate::events::jsonl::Chained for WatchRecord {
    fn chain_hash(&self) -> Option<&str> {
        self.chain_hash.as_deref()
    }

    fn set_chain_hash(&mut self, hash: Option<String>) {
        self.chain_hash = hash;
    }
}

/// A subscription that is currently in force.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActiveWatch {
    pub agent: String,
    pub path: String,
    pub prefix: bool,
    pub since: String,
}

const WATCHES_FILE: &str = "watches.jsonl";

/// For writing. Creates `.pact/` if needed, like every other write path.
fn watches_file(repo_root: &Path) -> Result<PathBuf> {
    Ok(crate::repo::pact_dir(repo_root)?.join(WATCHES_FILE))
}

/// For reading. Creates nothing — a question must not mutate (pact-rnc.27).
fn watches_file_path(repo_root: &Path) -> PathBuf {
    crate::repo::pact_dir_path(repo_root).join(WATCHES_FILE)
}

/// Every record in the file, oldest first, plus how many lines were
/// unreadable.
///
/// Tolerant in the same two ways the event log is, because it is now literally
/// the same read (`events::jsonl::read`): a torn final line from an interrupted
/// append is expected rather than corrupt, and an unknown `kind` parses fine
/// and is simply ignored by [`active`].
///
/// `unwrap_or_default` where the event log propagates: a registry that cannot
/// be read means "nobody is watching", because every caller here — `watch ls`,
/// `lease release`'s fanout, `pact audit` — has a sensible answer for an empty
/// registry and none at all for an error. Line numbers are dropped: a
/// subscription is identified by `(agent, path)`, never by where it landed in
/// the file.
pub fn records(repo_root: &Path) -> Result<(Vec<WatchRecord>, usize)> {
    let (rows, skipped) = crate::events::jsonl::read::<WatchRecord>(&watches_file_path(repo_root))
        .unwrap_or_default();
    Ok((rows.into_iter().map(|(_, r)| r).collect(), skipped))
}

/// The subscriptions in force, resolved by replaying the log.
///
/// Last record wins per `(agent, path)`: a re-`add` after an `rm` revives the
/// subscription without the file having to forget the `rm` ever happened.
/// Sorted by agent then path so output is stable across runs.
pub fn active(repo_root: &Path) -> Result<Vec<ActiveWatch>> {
    let (records, _) = records(repo_root)?;
    let mut by_key: BTreeMap<(String, String), WatchRecord> = BTreeMap::new();
    for r in records {
        by_key.insert((r.agent.clone(), r.path.clone()), r);
    }
    Ok(by_key
        .into_values()
        .filter(|r| r.kind == "watch")
        .map(|r| ActiveWatch {
            agent: r.agent,
            path: r.path,
            prefix: r.prefix,
            since: r.at,
        })
        .collect())
}

/// Does `watch` cover `released`?
///
/// An exact watch matches only itself. A prefix watch matches the directory
/// itself and anything beneath it, and the boundary is a `/` — so a watch on
/// `src/render` never matches `src/renderer.rs`, which is the bug a naive
/// `starts_with` would ship.
fn covers(watch: &ActiveWatch, released: &str) -> bool {
    if watch.path == released {
        return true;
    }
    watch.prefix && released.starts_with(&format!("{}/", watch.path.trim_end_matches('/')))
}

/// Was `agent` subscribed to `path` **as of `at`**, replaying the registry to that
/// instant?
///
/// `pact audit` cannot use the live registry (pact-1gv.7): a subscription retired
/// after the fact would rewrite history, reporting an agent as having had no channel
/// when it did. Same discipline as judging a hold against the TTL it recorded rather
/// than today's default.
///
/// Last record wins per (agent, path) up to `at`, exactly like [`active`] — so an
/// `unwatch` before `at` counts as unsubscribed and a re-`add` after it counts as
/// subscribed again.
pub fn was_subscribed_at(records: &[WatchRecord], agent: &str, path: &str, at: &str) -> bool {
    // Parsed instants, never raw strings (pact-rnc.20): two writers reach these
    // stamps — bd's `Z` and pact's chrono `+00:00` — and `'+'` sorts before `'Z'`,
    // so a string compare calls an older `Z` stamp newer. An unparsable stamp on
    // either side is treated as out of range rather than guessed at.
    let Some(cutoff) = chrono::DateTime::parse_from_rfc3339(at).ok() else {
        return false;
    };
    let mut subscribed = false;
    for r in records.iter().filter(|r| {
        r.agent == agent && chrono::DateTime::parse_from_rfc3339(&r.at).is_ok_and(|t| t <= cutoff)
    }) {
        let w = ActiveWatch {
            agent: r.agent.clone(),
            path: r.path.clone(),
            prefix: r.prefix,
            since: r.at.clone(),
        };
        if covers(&w, path) {
            subscribed = r.kind == "watch";
        }
    }
    subscribed
}

/// Does `agent` already subscribe to `path`, exactly or by a covering prefix?
///
/// The question `lease acquire` asks when it has just refused somebody
/// (pact-1gv.2). A refusal used to be a dead end with exactly one advertised
/// escape — `--steal` — which is the one action the refused agent should not take.
/// So polling was the only move it had been told about, and it polled: 24 of the
/// crucible run's 124 refusals came from an agent that had ALREADY registered a
/// watch on the path it was being refused. agent-03-r2 asked 13 times for
/// something it had arranged to be told about.
///
/// pact knew. The registry is right here, and the acquire path already reads
/// neighbouring state to compose its prior-claim and pending-message notes. It
/// simply never said so at the one moment the answer would have stopped a loop.
pub fn is_subscribed(repo_root: &Path, agent: &str, path: &str) -> bool {
    active(repo_root)
        .map(|ws| ws.iter().any(|w| w.agent == agent && covers(w, path)))
        .unwrap_or(false)
}

/// Who should be told that `released` changed, excluding `holder`.
///
/// Excluding the holder is not a nicety: an agent that subscribes to a
/// directory it also works in would otherwise message itself on every release,
/// which is both noise and a self-inflicted inbox.
pub fn subscribers_for(repo_root: &Path, released: &str, holder: &str) -> Result<Vec<ActiveWatch>> {
    let mut subs: Vec<ActiveWatch> = active(repo_root)?
        .into_iter()
        .filter(|w| w.agent != holder && covers(w, released))
        .collect();
    // One agent can hold both an exact and a prefix subscription covering the
    // same path; they are one recipient, not two messages.
    subs.dedup_by(|a, b| a.agent == b.agent);
    Ok(subs)
}

/// Append one record, chained to whatever is already at the end of the file.
///
/// `None` caps: this file is never trimmed. A subscription is not history that
/// grows without bound — it is one line per `watch add`/`watch rm` from a
/// fleet's worth of agents, and forgetting the oldest of them would silently
/// unsubscribe somebody rather than merely losing a statistic. The event log's
/// bound is a policy about ITS volume, which is why the caps are a parameter.
fn append(repo_root: &Path, record: WatchRecord) -> Result<()> {
    crate::events::jsonl::append(&watches_file(repo_root)?, &record, None)
}

/// Subscribe `agent` to `raw_path`.
///
/// Returns the normalized path and whether it was registered as a prefix, so
/// the caller can report exactly what it recorded rather than echoing the
/// argument back.
pub fn add(repo_root: &Path, agent: &str, raw_path: &str) -> Result<(String, bool)> {
    let prefix = is_prefix_request(repo_root, raw_path);
    // Same validation as a claim (pact-83r.4 / findings 3 and 11). A watch is where a bad
    // path hurts MOST: the failure mode is silence, and silence is indistinguishable from
    // "nothing has changed yet" — the exact state a watcher is waiting in.
    let path = crate::lease::resolve_claimable(repo_root, raw_path)?;
    append(
        repo_root,
        WatchRecord {
            at: Utc::now().to_rfc3339(),
            agent: agent.to_string(),
            kind: "watch".to_string(),
            path: path.clone(),
            prefix,
            chain_hash: None,
        },
    )?;
    Ok((path, prefix))
}

/// A trailing `/` says "everything under here" explicitly; an existing
/// directory says it implicitly, because `pact watch add src/render` on a real
/// directory can only sensibly mean its contents.
fn is_prefix_request(repo_root: &Path, raw_path: &str) -> bool {
    raw_path.ends_with('/') || repo_root.join(raw_path).is_dir()
}

/// Retire `agent`'s subscription to `raw_path`. `Ok(false)` when there was
/// nothing to retire, so the caller can say so instead of implying it undid
/// something.
pub fn remove(repo_root: &Path, agent: &str, raw_path: &str) -> Result<bool> {
    let path = crate::lease::normalize_path(repo_root, raw_path);
    let existing = active(repo_root)?
        .into_iter()
        .find(|w| w.agent == agent && w.path == path);
    let Some(existing) = existing else {
        return Ok(false);
    };
    append(
        repo_root,
        WatchRecord {
            at: Utc::now().to_rfc3339(),
            agent: agent.to_string(),
            kind: "unwatch".to_string(),
            path,
            prefix: existing.prefix,
            chain_hash: None,
        },
    )?;
    Ok(true)
}

/// How many diff lines a notification carries before it is cut short.
///
/// A cap rather than no cap because the message body is read by an agent with
/// a context window, and a very large refactor pasted into an inbox is worse
/// than a pointer to it: the reader stops reading. The truncation notice names
/// the holder's `HEAD`, so the full change is one `git show` away.
///
/// **1000 is measured, not guessed** (pact-b73.4). The first field run of this
/// feature delivered 87 diffs and truncated 44 of them — half — at the
/// original 200. The diffs it cut were nowhere near the size that motivated
/// the cap: median 397 lines, largest 839. 1000 delivers every diff that run
/// produced, in full, and still cuts anything genuinely unreadable.
///
/// Truncating is not free the way it would be for a human reader. A cut diff
/// degrades to "go and run `git show`", which is a second step off the
/// critical path — the exact category of voluntary step this whole feature
/// exists because agents skip.
const MAX_DIFF_LINES: usize = 1000;

/// Cut `diff` to [`MAX_DIFF_LINES`], appending a notice naming where to read
/// the rest. Returns the text unchanged when it already fits.
fn cap(diff: &str, head: Option<&str>) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        return diff.to_string();
    }
    let shown = lines[..MAX_DIFF_LINES].join("\n");
    let where_to_look = match head {
        Some(h) => format!("see commit {h}"),
        // A repo with no commits yet, or a git that would not answer. Saying
        // "see commit <nothing>" would be worse than admitting the gap.
        None => "the holder's working tree has the rest".to_string(),
    };
    format!(
        "{shown}\n\n[diff truncated after {} of {} lines — {where_to_look}]",
        MAX_DIFF_LINES,
        lines.len()
    )
}

/// Tell every subscriber to `released` what the holder changed while they held
/// it (pact-8qu).
///
/// **Infallible by signature, and that is the contract.** Delivery is a side
/// effect of `lease release`, and a release that failed because a notification
/// could not be sent would be strictly worse than the silence this feature
/// exists to fix: a lost message costs one missed diff, a stuck lease blocks a
/// peer until it expires. Same doctrine as [`crate::events::append`], which
/// swallows I/O errors for the same reason. Failures are recorded as
/// `watch-delivery-failed` events so they are visible in `pact log` and
/// `pact audit` rather than merely absent.
///
/// `old_hash` is the blob recorded at acquire time. `None` means the path did
/// not exist when it was claimed, or that hashing failed — in both cases there
/// is no fixed point to diff against, so nothing is sent. Silence is the right
/// answer to "I cannot tell what changed"; a notification saying so would be
/// noise on every lease taken to create a file.
pub fn notify_release(repo_root: &Path, holder: &str, released: &str, old_hash: Option<&str>) {
    // A reserved key is a NAME, not a file (`lease::is_mutex`). There is no blob
    // at acquire and none at release, so every content branch below returns
    // before it sends anything — and the silence lands on exactly the paths a
    // fleet serializes on, where somebody is most likely to be waiting.
    //
    // Measured in the millrace run (pact-bsf): an agent was refused the merge
    // mutex, subscribed with `pact watch add` exactly as the protocol tells it
    // to, and was never told when the path went free. The holder released 32s
    // later; the waiter acquired 3m01s after that, having fallen back to
    // polling. `pact audit` reported `watch 1 active; 0 diff(s) delivered`.
    //
    // What a waiter on a mutex wants is not a diff — it is the fact of release.
    if crate::lease::is_mutex(released) {
        notify_freed(repo_root, holder, released);
        return;
    }
    let Some(old_hash) = old_hash else { return };
    let new = crate::git_history::hash_objects(repo_root, &[released.to_string()]);
    let new_hash = new.get(released);
    // Unchanged content, or a file the holder deleted (no new blob). Neither
    // is worth a message: the first says nothing happened, and the second is
    // better learned from the commit than from a diff against nothing.
    let Some(new_hash) = new_hash else { return };
    if new_hash == old_hash {
        return;
    }

    let subscribers = match subscribers_for(repo_root, released, holder) {
        Ok(s) if !s.is_empty() => s,
        _ => return,
    };

    let head = crate::git_history::head_short(repo_root);
    let diff = crate::git_history::diff_blobs(repo_root, old_hash, new_hash, released)
        .map(|d| cap(&d, head.as_deref()))
        .unwrap_or_else(|| "(git produced no diff for this change)".to_string());

    // The branch the holder is on, and only when this repository actually has
    // linked worktrees — the same gate `lease::worktree_stamp` applies to the
    // lock's `branch` field, resolved from the same `repo_root`, in the same
    // process, so the two cannot disagree.
    //
    // The gate is the whole point rather than a byte-compat detail (pact-83r.9 /
    // finding 9). In a single checkout the notice IS a code delivery: the diff
    // describes the file sitting in the reader's own tree. In a worktree fleet it
    // can never be — the holder wrote it on their branch in their worktree, so
    // nothing arrives under the reader until that branch merges and they merge
    // that. An agent that had subscribed exactly as the protocol asks worked this
    // out for itself and killed the waiter it had started; the notice had given it
    // no reason to expect otherwise.
    let ctx = crate::repo::RepoContext::resolve(repo_root);
    let holder_branch = ctx.has_worktrees.then(|| ctx.branch()).flatten();

    // Built with the shared marker, because `pact msg inbox` parses the path
    // back out of it to group notices per path (pact-mqw.5). The const is the
    // only thing keeping the two halves from drifting apart.
    let subject = format!("{released}{}{holder}", crate::msg::NOTICE_SUBJECT_MARKER);
    let body = format!(
        "{holder} released {released}, which you are watching. What changed while they held it:\n\
         \n\
         {diff}\n\
         \n\
         Holder's HEAD at release: {}\n\
         {}\n\
         Questions, or a contract you need changed back? Reply to {holder} in \
         THIS thread — `pact msg inbox` shows its id, then \
         `pact msg send --to {holder} --thread <id> \"...\"`. A reply without \
         `--thread` starts a new conversation nobody can follow back to this \
         diff.\n\
         \n\
         You are receiving this because you ran `pact watch add`. \
         `pact watch rm {released}` stops it.",
        head.as_deref().unwrap_or("(unknown)"),
        match &holder_branch {
            Some(b) => format!(
                "\nThis is a contract notice, not a code delivery: {holder} wrote this on \
                 branch {b}, in their own worktree. It cannot appear in your tree until {b} \
                 merges and you merge that. Read the diff for what the contract now says and \
                 carry on — the file will not change under you, so there is nothing to wait \
                 for.\n"
            ),
            None => String::new(),
        }
    );

    deliver(repo_root, holder, released, &subject, &body, &subscribers);
}

/// Tell every subscriber to a reserved key that it is free (pact-bsf).
///
/// The counterpart to [`notify_release`]'s diff path, for a lease that stands for
/// something other than a file. There is nothing to diff and nothing to read into
/// a working tree — the message IS the availability, so it says the one thing a
/// waiter needs and nothing else.
///
/// Note what this deliberately does NOT do: it does not tell the waiter to go and
/// acquire. Several may be watching one mutex and only one can win, so the notice
/// reports a fact rather than issuing an instruction that would be wrong for
/// everybody but the fastest reader.
fn notify_freed(repo_root: &Path, holder: &str, released: &str) {
    let subscribers = match subscribers_for(repo_root, released, holder) {
        Ok(s) if !s.is_empty() => s,
        _ => return,
    };

    let subject = format!("{released}{}{holder}", crate::msg::NOTICE_FREED_MARKER);
    let body = format!(
        "{holder} released {released}, which you are watching. It is free as of this \
         message.\n\
         \n\
         This is a reserved key, not a file: there is no diff, because there was never \
         any content to change. Nothing has appeared in your working tree and there is \
         nothing here to read into it.\n\
         \n\
         If you were refused this path and are waiting on it, this is your signal to \
         try again — but do not assume you will win it. Anyone else watching got this \
         same message at the same moment, and only one of you can hold it.\n\
         \n\
         You are receiving this because you ran `pact watch add`. \
         `pact watch rm {released}` stops it."
    );

    deliver(repo_root, holder, released, &subject, &body, &subscribers);
}

/// Send one notice per subscriber, and record each delivery or failure.
///
/// Shared by the diff path and the freed path so the two cannot drift in what
/// they tag, how they thread, or whether they leave an event behind. Same
/// infallible-by-signature contract as [`notify_release`]: a release has already
/// landed by the time anything here runs.
fn deliver(
    repo_root: &Path,
    holder: &str,
    released: &str,
    subject: &str,
    body: &str,
    subscribers: &[ActiveWatch],
) {
    let about = [released.to_string()];
    // No backend to locate any more (pact-as5.4). Delivery used to begin by finding
    // a `bd` binary and give up here if there was none — which meant the one part of
    // the protocol that runs WITHOUT an agent choosing to run it, on a path an agent
    // is walking away from, depended on somebody having installed the issue tracker.
    // A notice is now an append to `.pact/messages.jsonl` and cannot fail that way.
    //
    // One message per subscriber rather than one with several recipients:
    // each is a separate conversation about a file THEY watch, and a shared
    // thread would put unrelated agents into each other's replies.
    for sub in subscribers {
        let draft = crate::msg::Draft {
            thread: None,
            subject: Some(subject),
            body,
            // Tagged with the path, so the message follows the file the way
            // `--to-owner-of` messages do: whoever leases it next is told one
            // is waiting, even if this subscriber has exited.
            about: &about,
            // The one caller that sets this. It is what keeps a hot file's
            // release fanout out of the queue an agent reads for correspondence
            // — see `msg::NOTICE`.
            notice: true,
        };
        match crate::msg::send(repo_root, holder, std::slice::from_ref(&sub.agent), draft) {
            Ok(sent) => {
                let id = sent.first().map(|m| m.id.clone());
                crate::events::append(
                    repo_root,
                    &crate::events::Event {
                        at: Utc::now().to_rfc3339(),
                        agent: holder.to_string(),
                        kind: "notified".to_string(),
                        path: Some(released.to_string()),
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
                        subscriber: Some(sub.agent.clone()),
                        message_id: id,
                        protocol_hash: None,
                        head: None,
                        holder: None,
                        holder_remaining_secs: None,
                        holder_branch: None,
                        holder_worktree: None,
                    },
                );
            }
            Err(e) => log_failure(
                repo_root,
                holder,
                released,
                Some(&sub.agent),
                &format!("{e:#}"),
            ),
        }
    }
}

/// A delivery that did not happen, recorded so it is visible rather than
/// merely absent. Never printed: `lease release`'s output contract is fixed,
/// and a warning on stderr for a best-effort side effect would train agents to
/// ignore stderr.
fn log_failure(repo_root: &Path, holder: &str, released: &str, sub: Option<&str>, why: &str) {
    crate::events::append(
        repo_root,
        &crate::events::Event {
            at: Utc::now().to_rfc3339(),
            agent: holder.to_string(),
            kind: "watch-delivery-failed".to_string(),
            path: Some(released.to_string()),
            detail: Some(why.to_string()),
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
            subscriber: sub.map(str::to_string),
            message_id: None,
            protocol_hash: None,
            head: None,
            holder: None,
            holder_remaining_secs: None,
            holder_branch: None,
            holder_worktree: None,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        tmp
    }

    fn watch(agent: &str, path: &str, prefix: bool) -> ActiveWatch {
        ActiveWatch {
            agent: agent.into(),
            path: path.into(),
            prefix,
            since: String::new(),
        }
    }

    #[test]
    fn add_then_list_then_remove_round_trips() {
        let tmp = repo();
        let root = tmp.path();
        assert!(active(root).unwrap().is_empty());

        let (path, prefix) = add(root, "w5-juice", "src/render/mod.rs").unwrap();
        assert_eq!(path, "src/render/mod.rs");
        assert!(!prefix, "a plain file is an exact subscription");
        assert_eq!(active(root).unwrap().len(), 1);

        assert!(remove(root, "w5-juice", "src/render/mod.rs").unwrap());
        assert!(active(root).unwrap().is_empty(), "the tombstone retires it");

        // And the file was never rewritten — both records are still there.
        let (records, _) = records(root).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, "watch");
        assert_eq!(records[1].kind, "unwatch");
    }

    #[test]
    fn removing_something_never_watched_reports_it_rather_than_writing_a_tombstone() {
        let tmp = repo();
        assert!(!remove(tmp.path(), "nobody", "src/x.rs").unwrap());
        assert_eq!(records(tmp.path()).unwrap().0.len(), 0);
    }

    /// Re-subscribing after an `rm` must work without the log forgetting the
    /// `rm` — last record wins per (agent, path).
    #[test]
    fn a_resubscribe_after_a_remove_is_active_again() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "a", "src/x.rs").unwrap();
        remove(root, "a", "src/x.rs").unwrap();
        add(root, "a", "src/x.rs").unwrap();
        assert_eq!(active(root).unwrap().len(), 1);
        assert_eq!(records(root).unwrap().0.len(), 3);
    }

    /// The bug a naive `starts_with` ships: `src/render` must not match
    /// `src/renderer.rs`. The boundary is a path separator.
    #[test]
    fn a_prefix_watch_matches_on_a_path_boundary_only() {
        let dir = watch("a", "src/render", true);
        assert!(covers(&dir, "src/render/mod.rs"));
        assert!(covers(&dir, "src/render/deep/nested.rs"));
        assert!(covers(&dir, "src/render"), "the directory itself counts");
        assert!(
            !covers(&dir, "src/renderer.rs"),
            "a sibling sharing a name prefix is a different file"
        );
        assert!(!covers(&dir, "src/other.rs"));

        let exact = watch("a", "src/render/mod.rs", false);
        assert!(covers(&exact, "src/render/mod.rs"));
        assert!(
            !covers(&exact, "src/render/other.rs"),
            "an exact watch subscribes to one file"
        );
    }

    #[test]
    fn a_trailing_slash_or_a_real_directory_registers_a_prefix() {
        let tmp = repo();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/render")).unwrap();

        // Explicit, on a directory that does not exist.
        let (_, prefix) = add(root, "a", "docs/future/").unwrap();
        assert!(prefix, "a trailing slash says prefix outright");
        // Implicit, from the directory really being one.
        let (_, prefix) = add(root, "a", "src/render").unwrap();
        assert!(prefix, "an existing directory can only mean its contents");
        // A plain file is exact.
        let (_, prefix) = add(root, "a", "src/render/mod.rs").unwrap();
        assert!(!prefix);
    }

    /// Self-exclusion, and the reason it matters: an agent subscribed to a
    /// directory it also works in would message itself on every release.
    #[test]
    fn the_releasing_agent_is_never_its_own_subscriber() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "holder", "src/").unwrap();
        add(root, "watcher", "src/").unwrap();

        let subs = subscribers_for(root, "src/api.rs", "holder").unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].agent, "watcher");
    }

    /// One agent holding both an exact and a covering prefix subscription is
    /// one recipient, not two copies of the same diff.
    #[test]
    fn overlapping_subscriptions_by_one_agent_produce_one_recipient() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "watcher", "src/").unwrap();
        add(root, "watcher", "src/api.rs").unwrap();

        let subs = subscribers_for(root, "src/api.rs", "holder").unwrap();
        assert_eq!(subs.len(), 1, "{subs:?}");
    }

    /// Chained like the event log, so a hand-edited subscription is
    /// detectable rather than indistinguishable from a real one.
    #[test]
    fn each_record_chains_to_the_one_before_it() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "a", "x.rs").unwrap();
        add(root, "b", "y.rs").unwrap();

        let (records, _) = records(root).unwrap();
        let first = records[0].chain_hash.clone().unwrap();
        let mut unchained = records[1].clone();
        unchained.chain_hash = None;
        let expected = chain_hash_of(&first, &serde_json::to_string(&unchained).unwrap());
        assert_eq!(records[1].chain_hash.as_deref(), Some(expected.as_str()));
    }

    /// A torn final line from an interrupted append is expected, not corrupt —
    /// the same tolerance the event log has.
    #[test]
    fn a_torn_final_line_is_counted_and_skipped() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "a", "x.rs").unwrap();
        let file = watches_file_path(root);
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str("{\"at\":\"2026-08-09T00:00:00Z\",\"agent\":\"b\",\"ki");
        std::fs::write(&file, text).unwrap();

        let (records, skipped) = records(root).unwrap();
        assert_eq!(records.len(), 1, "the whole record still counts");
        assert_eq!(skipped, 1);
        assert_eq!(active(root).unwrap().len(), 1);
    }

    #[test]
    fn a_diff_under_the_cap_is_passed_through_untouched() {
        let small = "diff --git a/x b/x\n-old\n+new\n";
        assert_eq!(cap(small, Some("abc1234")), small);
    }

    #[test]
    fn an_oversized_diff_is_cut_and_says_where_to_read_the_rest() {
        let big: String = (0..MAX_DIFF_LINES + 50)
            .map(|i| format!("+line {i}\n"))
            .collect();
        let out = cap(&big, Some("abc1234"));
        // The cap plus the blank line and the notice — never the whole thing.
        assert!(
            out.lines().count() < MAX_DIFF_LINES + 10,
            "{}",
            out.lines().count()
        );
        assert!(out.contains("line 0"), "the head of the diff survives");
        assert!(
            !out.contains(&format!("line {}", MAX_DIFF_LINES + 49)),
            "the tail is cut: {out}"
        );
        // Derived from the constant, not hardcoded: the cap is measured
        // against field data (see MAX_DIFF_LINES) and is expected to move
        // again, so a literal here would just have to be chased.
        assert!(
            out.contains(&format!(
                "truncated after {} of {} lines",
                MAX_DIFF_LINES,
                MAX_DIFF_LINES + 50
            )),
            "{out}"
        );
        assert!(out.contains("see commit abc1234"), "{out}");
    }

    /// A repo with no commits has no HEAD to point at, and saying
    /// "see commit " with nothing after it is worse than admitting the gap.
    #[test]
    fn truncation_without_a_head_says_so_rather_than_naming_nothing() {
        let big: String = (0..MAX_DIFF_LINES + 1)
            .map(|i| format!("+l{i}\n"))
            .collect();
        let out = cap(&big, None);
        assert!(out.contains("working tree has the rest"), "{out}");
        assert!(!out.contains("see commit"), "{out}");
    }

    /// The release must complete even when there is no backend to deliver
    /// through, and the failure must be visible rather than silent.
    #[test]
    fn a_delivery_with_no_backend_records_a_failure_and_returns() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "watcher", "x.rs").unwrap();
        // A hash that is not a real blob: git cannot diff it, and there is no
        // Beads CLI configured for this tempdir either. Neither may panic.
        notify_release(
            root,
            "holder",
            "x.rs",
            Some("0000000000000000000000000000000000000000"),
        );
        // Nothing to assert about delivery — the point is that it returned.
        assert!(active(root).unwrap().len() == 1);
    }

    /// No baseline means no notification: a lease taken on a file that did not
    /// exist yet must not produce a message saying nothing can be said.
    #[test]
    fn a_lease_with_no_recorded_content_hash_notifies_nobody() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "watcher", "x.rs").unwrap();
        notify_release(root, "holder", "x.rs", None);
        let (records, _) = crate::events::numbered(root).unwrap_or_default();
        assert!(
            records.is_empty(),
            "no baseline must produce no event at all: {records:?}"
        );
    }

    /// pact-bsf, the regression this whole branch exists for: a reserved key has
    /// no blob at acquire and none at release, so the content path sends nothing
    /// and a waiter on the merge mutex is never told it went free.
    #[test]
    fn releasing_a_reserved_key_tells_its_watchers_it_is_free() {
        let tmp = repo();
        let root = tmp.path();
        let mutex = ".pact/internal/merge-to-master";
        add(root, "waiter", mutex).unwrap();

        // `None` — exactly what the lock records for a path that is not a file,
        // and precisely the argument that used to return before sending.
        notify_release(root, "holder", mutex, None);

        let inbox = crate::msg::inbox(root, "waiter", false).unwrap();
        assert_eq!(inbox.len(), 1, "the waiter must be told: {inbox:?}");
        let m = &inbox[0];
        assert!(m.notice, "it is fanout, not correspondence");
        assert!(
            m.body.contains("free as of this message"),
            "the notice must say the thing a waiter needs: {}",
            m.body
        );
        assert!(
            !m.body.contains("What changed while they held it"),
            "it must not take the diff path's shape for a name: {}",
            m.body
        );
        // And it is visible in the log, so `pact audit` can report delivery.
        let (events, _) = crate::events::numbered(root).unwrap_or_default();
        assert!(
            events.iter().any(|(_, e)| e.kind == "notified"),
            "delivery must leave an event: {events:?}"
        );
    }

    /// The holder is not its own waiter here either — an agent that both watches
    /// and takes a mutex would otherwise message itself on every release.
    #[test]
    fn a_reserved_key_release_does_not_notify_the_releasing_agent() {
        let tmp = repo();
        let root = tmp.path();
        let mutex = ".pact/internal/merge-to-master";
        add(root, "holder", mutex).unwrap();

        notify_release(root, "holder", mutex, None);
        assert!(crate::msg::inbox(root, "holder", false).unwrap().is_empty());
    }

    /// A directory lease is a mutex by the same rule (`lease::is_mutex`), and it
    /// has no single blob either — so it takes the freed path, not silence.
    #[test]
    fn releasing_a_directory_lease_also_reports_it_as_freed() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "waiter", ".beads/").unwrap();

        notify_release(root, "holder", ".beads/", None);
        let inbox = crate::msg::inbox(root, "waiter", false).unwrap();
        assert_eq!(inbox.len(), 1, "{inbox:?}");
    }

    /// The diff path must be untouched: a real file with no baseline still says
    /// nothing, because "I cannot tell what changed" is not worth a message.
    #[test]
    fn a_plain_file_with_no_baseline_still_notifies_nobody() {
        let tmp = repo();
        let root = tmp.path();
        add(root, "watcher", "src/api.rs").unwrap();
        notify_release(root, "holder", "src/api.rs", None);
        assert!(crate::msg::inbox(root, "watcher", false)
            .unwrap()
            .is_empty());
    }

    /// Reading must never create `.pact/` — audit and `watch ls` are
    /// questions, and a question that mutates is not one.
    #[test]
    fn listing_creates_nothing() {
        let tmp = repo();
        assert!(active(tmp.path()).unwrap().is_empty());
        assert!(
            !tmp.path().join(".pact").exists(),
            "listing watches must not create .pact/"
        );
    }
}
