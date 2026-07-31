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

use std::cmp::Ordering;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::beads::BeadsCli;
use crate::output;

/// Label prefix marking "this agent has read this message".
const READ_BY: &str = "read-by-";

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
/// part-way through leaves the earlier recipients' messages sent; the error
/// says which, and `sent()` lists them, so nobody has to re-send blind.
pub fn send(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    to: &[String],
    thread: Option<&str>,
    subject: Option<&str>,
    body: &str,
) -> Result<Vec<Message>> {
    if to.is_empty() {
        anyhow::bail!("no recipients — `msg send` needs at least one --to");
    }
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
    for recipient in to {
        let issue = create(
            cli,
            repo_root,
            agent,
            recipient,
            thread_id.as_deref(),
            &title,
            body,
        )
        .with_context(|| match messages.len() {
            0 => format!("sending to {recipient}: nothing was sent"),
            n => format!(
                "sending to {recipient}: {n} of {} recipient(s) already got this ({}) — \
                 do not re-send to them",
                to.len(),
                messages
                    .iter()
                    .map(|m| m.to.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;
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
    }
    Ok(messages)
}

/// `bd create` args for one message bead. Owned Strings because they are all
/// interpolated; see the module docs for why `--no-inherit-labels` is not
/// optional.
fn create_args(
    to: &str,
    parent: Option<&str>,
    agent: &str,
    title: &str,
    body: &str,
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        "--type=message".to_string(),
        "--json".to_string(),
        // A child must not inherit the parent's read-by-* labels, or a reply
        // would be born already "read" by whoever read the message above it.
        "--no-inherit-labels".to_string(),
        format!("--title={title}"),
        format!("--description={body}"),
        format!("--assignee={to}"),
        // Records who (in pact's own identity scheme) sent this, in bd's audit
        // trail — this is what `from` and `sent()` read back.
        format!("--actor={agent}"),
    ];
    if let Some(p) = parent {
        args.push(format!("--parent={p}"));
    }
    args
}

fn create(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    to: &str,
    parent: Option<&str>,
    title: &str,
    body: &str,
) -> Result<BdIssue> {
    let args = create_args(to, parent, agent, title, body);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let stdout = cli.run(repo_root, &borrowed)?;
    serde_json::from_str(&stdout).context("parsing `bd create --json` output")
}

pub fn inbox(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    unread_only: bool,
) -> Result<Vec<Message>> {
    let assignee_arg = format!("--assignee={agent}");
    let stdout = cli.run(
        repo_root,
        &["list", "--include-infra", "--json", &assignee_arg],
    )?;
    let mut messages = parse_messages(&stdout, Some(agent))?;

    if unread_only {
        messages.retain(|m| !m.read);
    }
    Ok(messages)
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

/// One `bd show <id> --json`, which returns an array even for a single id.
fn show(cli: &BeadsCli, repo_root: &Path, id: &str) -> Result<BdIssue> {
    let out = cli.run(repo_root, &["show", id, "--json"])?;
    let mut issues: Vec<BdIssue> =
        serde_json::from_str(&out).context("parsing `bd show --json` output")?;
    issues
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

/// The root message plus its direct replies, oldest first. Marks everything
/// shown as read for `agent`.
///
/// `id` may be any member of the thread, not just its root: a non-first recipient
/// of a fan-out send only ever sees her own child id, and reading it must give
/// her the whole conversation — and the root's id as the thread — rather than a
/// one-message "thread" whose id produces invisible grandchild replies.
pub fn read_thread(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    id: &str,
) -> Result<Vec<Message>> {
    let root = thread_root(cli, repo_root, id)?;
    let root_id = root.id.clone();

    let parent_arg = format!("--parent={root_id}");
    let list_out = cli.run(
        repo_root,
        &["list", "--include-infra", "--json", &parent_arg],
    )?;
    let replies: Vec<BdIssue> =
        serde_json::from_str(&list_out).context("parsing `bd list --json` output")?;

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

    Ok(all
        .into_iter()
        .map(|i| {
            let mut m = i.into_message(Some(&root_id), Some(agent));
            // Just labelled, so the pre-read snapshot above doesn't show it yet.
            if marked && !m.read {
                m.read = true;
                m.read_by.push(agent.to_string());
            }
            m
        })
        .collect())
}

/// The one place the `read-by-` label is spelled for writing; `into_message`
/// is the one place it is spelled for reading.
fn read_label(agent: &str) -> String {
    format!("{READ_BY}{agent}")
}

/// `bd label add <id>... read-by-<agent>` — one call for the whole thread, and
/// idempotent, so re-reading a thread is a no-op rather than a duplicate.
fn mark_read(cli: &BeadsCli, repo_root: &Path, agent: &str, issues: &[BdIssue]) -> Result<()> {
    let label = read_label(agent);
    let mut args = vec!["label", "add"];
    args.extend(issues.iter().map(|i| i.id.as_str()));
    args.push(&label);
    cli.run(repo_root, &args).map(|_| ())
}

/// Every message bead in the repo, regardless of recipient, oldest first.
///
/// `bd list` hides message beads unless `--include-infra` is passed and has no
/// `--type` filter, so `issue_type == "message"` is filtered client-side, same
/// as `inbox()`. There is no querying agent here, so `read` is resolved against
/// each message's own recipient.
pub fn all_messages(cli: &BeadsCli, repo_root: &Path) -> Result<Vec<Message>> {
    let stdout = cli.run(repo_root, &["list", "--include-infra", "--json"])?;
    parse_messages(&stdout, None)
}

/// `bd list --json` output -> message beads only, oldest first.
fn parse_messages(stdout: &str, viewer: Option<&str>) -> Result<Vec<Message>> {
    let issues: Vec<BdIssue> =
        serde_json::from_str(stdout).context("parsing `bd list --json` output")?;
    let mut messages: Vec<Message> = issues
        .into_iter()
        .filter(|i| i.issue_type == "message")
        .map(|i| i.into_message(None, viewer))
        .collect();
    messages.sort_by(oldest_first);
    Ok(messages)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let root = create_args("mascot-dev", None, "animator", "Alarmed loops", "body");
        assert!(
            !root.iter().any(|a| a.starts_with("--parent=")),
            "a new thread's root has no parent: {root:?}"
        );
        assert!(root.contains(&"--assignee=mascot-dev".to_string()));
        assert!(root.contains(&"--actor=animator".to_string()));

        // Second recipient, parented on the root bead the first create returned.
        let child = create_args(
            "tui-dev",
            Some("pact-wisp-a2u"),
            "animator",
            "Alarmed loops",
            "body",
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
            None,
            None,
            "body",
        )
        .expect_err("empty --to must not be accepted");
        assert!(err.to_string().contains("no recipients"), "{err}");
    }
}
