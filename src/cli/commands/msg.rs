//! `pact msg`: sending, reading, and every way a message is rendered.
//!
//! One file because the four actions are one conversation seen from four
//! angles — `send` warns about a recipient `inbox` would have shown you, and
//! `sent` is the inbox with the columns swapped. The two age thresholds below
//! are `send`'s alone and stay next to the warnings they gate.

use anyhow::{Context, Result};
use std::path::Path;

use crate::cli::util::{age_of, one_line, since, table};
use crate::cli::MsgAction;
use crate::lease::human_secs;
use crate::{agents, events, identity, lease, msg, output, repo};

/// How long a resolved recipient can have been silent before `msg send` says so.
/// Fifteen minutes is well past a normal think-and-edit gap and well short of a
/// session, so it flags an agent that has probably exited without nagging about
/// one that is merely busy.
const QUIET_AGENT_SECS: i64 = 15 * 60;

/// Past this, a suggested correction says how old it is. Below it, the name
/// alone — an age on a peer that acted seconds ago is noise, and the whole
/// point of the annotation is that a borderline suggestion can be judged.
/// Names older than `agents`' suggestion horizon are not offered at all.
const ANNOTATE_SUGGESTION_AGE_SECS: i64 = 15 * 60;

pub(in crate::cli) fn run_msg(
    cwd: &Path,
    agent_flag: Option<&str>,
    json: bool,
    action: MsgAction,
) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    // No backend probe, and no conflicting-store warning (pact-as5.3).
    //
    // Both existed because `pact msg` QUERIED a Beads store: the `locate()?` made
    // every message command exit 3 when `bd` was missing, and the warning explained
    // an "inbox empty" that was really a second, shadowed store being read
    // (pact-m7j.10.7). Messages live in `.pact/messages.jsonl` now, so neither fact
    // can affect an inbox, and repeating them here would be telling an agent about a
    // dependency this command no longer has. `pact doctor` still reports both.
    //
    // This is what makes exit 3 unreachable from every `msg` path — see the
    // exit-code table in README.md.
    let agent = identity::resolve_agent(agent_flag)?;
    match action {
        MsgAction::Send {
            to,
            to_owner_of,
            thread,
            subject,
            body,
            body_file,
            skip,
        } => {
            let body = match body_file {
                Some(p) => read_body(&p)?,
                None => body.unwrap_or_default(),
            };
            if body.trim().is_empty() {
                anyhow::bail!("empty message body — nothing to send");
            }
            // Normalized once, here, before anything compares it — pact-m7j.8.6:
            // `to_owner_of` is whatever the caller's own CWD made of it, and
            // both uses below need the SAME canonical spelling `acquire`
            // itself would have produced: the lookup just below (so a path
            // typed from a subdirectory still resolves to its real prior
            // owner) and, further down, the `about-<path>` labels this send
            // is tagged with (so a later `about_path` query — typed from yet
            // another CWD — still finds it). Reassigning rather than reading
            // through a second binding, so nothing downstream can reach the
            // un-normalized spelling by mistake.
            let to_owner_of: Vec<String> = to_owner_of
                .iter()
                .map(|p| lease::normalize_path(&root, p))
                .collect();
            // Resolve paths to the agents who last worked on them. This is
            // what makes a handoff survive its author: 51 of 59 messages in one
            // fleet run were never read, because they were addressed to
            // processes that had already exited rather than to the work itself
            // (pact-o38). A path outlives the agent holding it.
            let mut to = to;
            for path in &to_owner_of {
                match events::owner_of(&root, path)? {
                    Some(owner) if owner.agent == agent => {
                        output::warn(&format!(
                            "note: you are yourself the last agent to work on {path}; not adding a recipient"
                        ));
                    }
                    Some(owner) => {
                        // Say who the path resolved to and how stale they are.
                        // A resolved name looks like a delivered message, and
                        // it is not: every message to a live agent in the last
                        // fleet run was read, every message to an exited one
                        // was not. One agent worked around this by hand-adding
                        // `--to human` to all three of its sends; they were the
                        // only one who thought of it (pact-4tj).
                        let ago = age_of(&owner.at)
                            .map(human_secs)
                            .unwrap_or_else(|| "an unknown time".to_string());
                        let stale = age_of(&owner.at).is_some_and(|s| s > QUIET_AGENT_SECS);
                        output::warn(&format!(
                            "note: {path} resolved to {}, last seen {ago} ago{}",
                            owner.agent,
                            if stale {
                                " — they may have exited; whoever leases that path next                                  will be shown this message"
                            } else {
                                ""
                            }
                        ));
                        if !to.contains(&owner.agent) {
                            to.push(owner.agent);
                        }
                    }
                    None => anyhow::bail!(
                        "no agent has ever leased {path}, so it has no owner to address — \
                         `pact lease ls --all` lists every path pact knows"
                    ),
                }
            }
            if to.is_empty() {
                // Every `--to-owner-of` path resolved to the sender itself
                // (the self-owner branch above warned and added nothing), and
                // no explicit `--to` was given — clap requires one or the
                // other. Refusing outright used to strand the sender exactly
                // when `--to-owner-of` exists to save them from guessing a
                // name (pact-m7j.10.5, reproduced live: an agent that had
                // just taken over a path had no way to tell its previous
                // co-editor). `msg::send`'s about-<path> tagging attaches to
                // every `to_owner_of` path unconditionally, and
                // `messages_about()` surfaces it to whoever leases that path
                // next regardless of who `to` was — so addressing it to
                // `agents::HUMAN` still delivers it forward through that
                // pipeline instead of losing it.
                output::warn(&format!(
                    "note: every --to-owner-of path resolves to you; addressing to {} so the note \
                     still reaches whoever leases it next",
                    agents::HUMAN
                ));
                to.push(agents::HUMAN.to_string());
            }
            // pact-m7j.6.5: a replay of a partially-failed send names the
            // recipients who already got it (`already_sent` in the previous
            // attempt's `--json` error) so this one does not duplicate
            // delivery to them. Applied after every other recipient source
            // (`--to`, `--to-owner-of`, the HUMAN fallback above) has already
            // built the list, so `--skip` behaves the same regardless of how
            // a name got into `to`.
            if !skip.is_empty() {
                let before = to.len();
                to.retain(|r| !skip.contains(r));
                let skipped = before - to.len();
                if skipped > 0 {
                    output::warn(&format!(
                        "note: {skipped} recipient(s) skipped — already sent to them, not re-sending"
                    ));
                }
            }
            for recipient in &to {
                check_recipient(recipient)?;
            }
            // One registry lookup for all recipients, not one per --to.
            warn_if_unknown(&root, &to);
            let sent = msg::send(
                &root,
                &agent,
                &to,
                msg::Draft {
                    thread: thread.as_deref(),
                    subject: subject.as_deref(),
                    body: &body,
                    about: &to_owner_of,
                    // Always authored. There is deliberately no flag for
                    // "file this under machine noise" — that tag exists so an
                    // agent can trust that what is left IS correspondence.
                    notice: false,
                },
            )?;
            output::emit(json, &sent, |sent: &Vec<msg::Message>| {
                let root_msg = &sent[0]; // send() errors on an empty recipient list
                if sent.len() == 1 {
                    return format!(
                        "sent {} to {} (thread {})",
                        root_msg.id, root_msg.to, root_msg.thread
                    );
                }
                // The thread id ONCE, not once per recipient: the whole point
                // of pact-rnc.4 is that this is one conversation, so `msg read
                // <thread>` shows the announcement instead of N near-duplicates.
                format!(
                    "sent {} message(s) in thread {}\n{}",
                    sent.len(),
                    root_msg.thread,
                    sent.iter()
                        .map(|m| format!("  {} → {}", m.id, m.to))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            });
            Ok(())
        }
        MsgAction::Sent => {
            let messages = msg::sent(&root, &agent)?;
            output::emit(json, &messages, |messages: &Vec<msg::Message>| {
                if messages.is_empty() {
                    format!("{agent} has sent nothing yet")
                } else {
                    render_sent(messages)
                }
            });
            Ok(())
        }
        MsgAction::Inbox {
            unread_only,
            full,
            include_watch,
            watch_only,
        } => {
            let view = if watch_only {
                msg::WatchView::Only
            } else if include_watch {
                msg::WatchView::Include
            } else {
                msg::WatchView::Authored
            };
            let messages = msg::inbox(&root, &agent, unread_only)?;
            // `--json` is never coalesced: a machine can group for itself, and
            // collapsing nine deliveries into one entry would cost it their ids.
            // The flags choose which messages it sees and nothing else.
            let picked: Vec<&msg::Message> = messages
                .iter()
                .filter(|m| match view {
                    msg::WatchView::Authored => !m.notice,
                    msg::WatchView::Include => true,
                    msg::WatchView::Only => m.notice,
                })
                .collect();
            let (authored, notices) = msg::split_notices(&messages);
            output::emit(json, &picked, |_: &Vec<&msg::Message>| {
                render_inbox_view(&authored, &notices, view, full)
            });
            Ok(())
        }
        MsgAction::Read { id, brief } => {
            let thread = msg::read_thread(&root, &agent, &id)?;
            // `--brief` is a RENDERING flag: `--json` is pinned shape and stays
            // one object per recipient either way (pact-83r.8).
            output::emit(json, &thread, |thread: &Vec<msg::Message>| {
                let flat = thread.iter().collect::<Vec<_>>();
                if brief {
                    render_brief(&flat)
                } else {
                    render_full(&flat)
                }
            });
            Ok(())
        }
    }
}

/// Ceiling on the stdin read, not a latency target: a legitimate producer may be
/// slow, so this is generous on purpose. `PACT_STDIN_BODY_TIMEOUT_MS` overrides
/// it — the only way a test can prove the bound exists without waiting it out.
fn stdin_body_timeout() -> std::time::Duration {
    match std::env::var("PACT_STDIN_BODY_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => std::time::Duration::from_secs(60),
    }
}

/// `--body-file -` must not be able to block forever (pact-83r.5).
///
/// UNCONFIRMED: a fleet reported this hanging past 120 s and wedging the shell,
/// and it does not reproduce on 0.9.4 — but it cannot be reproduced here either,
/// because the report's precondition is a tty on stdin and no test environment
/// has one. Guarded anyway, because of *where* the hang is: `msg send` is the
/// tool an agent uses to report that it is blocked, so a hang there is the one
/// an agent cannot report its way out of.
///
/// Two guards, for the two ways the read never returns. A tty means no producer
/// is attached at all — that is a mistake, not slowness, so it is refused
/// immediately and names the alternative. Everything else gets a bounded read;
/// the reading thread is left blocked, which costs nothing because the process
/// is about to exit either way.
fn read_stdin_body() -> Result<String> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        anyhow::bail!(
            "--body-file - reads the body from stdin, but stdin is a terminal: \
             nothing is feeding it, so the read would never return. Pipe the body \
             in, or write it to a file and use --body-file <path>."
        );
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = tx.send(std::io::stdin().read_to_string(&mut s).map(|_| s));
    });
    let timeout = stdin_body_timeout();
    match rx.recv_timeout(timeout) {
        Ok(r) => r.context("reading message body from stdin"),
        Err(_) => anyhow::bail!(
            "stdin gave no end of input after {}s — whatever is feeding \
             --body-file - is not finishing. Write the body to a file and use \
             --body-file <path>; nothing was sent.",
            timeout.as_secs_f32(),
        ),
    }
}

/// `-` means stdin, so a multi-paragraph body full of quotes and backslashes
/// never has to survive a shell (pact-rnc.3).
fn read_body(path: &str) -> Result<String> {
    let raw = if path == "-" {
        read_stdin_body()?
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading message body from {path}"))?
    };
    // A file ends in a newline; that is punctuation, not content. Exactly one,
    // though: `trim_end()` ate trailing blank lines out of a deliberately
    // formatted body — an ASCII table, an indented code block — and --body-file
    // exists to promise byte fidelity (pact-rnc.25). An all-whitespace body is
    // still refused, by the caller's `body.trim().is_empty()` check.
    Ok(raw.strip_suffix('\n').unwrap_or(&raw).to_string())
}

/// A recipient that violates pact's own identity grammar is not merely unseen,
/// it is impossible: no process could ever pass that name to `--agent` or
/// `PACT_AGENT`, so the message can never be read by anyone. Refuse it, instead
/// of warning about something no send will ever fix (pact-rnc.5).
fn check_recipient(to: &str) -> Result<()> {
    identity::validate(to).with_context(|| {
        format!(
            "cannot send to {to:?}: no agent can run pact under that name, so nobody could read it"
        )
    })
}

/// pact-rnc.5: warn on stderr, then send anyway and exit 0. A bootstrapping
/// fleet legitimately messages agents that have not acted yet, so this must
/// never become a wall — and a lookup that only feeds a warning must never
/// break a send, hence the swallowed error.
fn warn_if_unknown(root: &Path, to: &[String]) {
    let known = agents::list(root).unwrap_or_default();
    for recipient in to {
        if let Some(warning) = unknown_recipient_warning(&known, recipient) {
            output::warn(&warning);
        }
    }
}

/// The warning text, or `None` when `to` has acted here. Split out from the
/// stderr write so the two ways this check used to go quiet — a name that was
/// only ever addressed, and an empty registry on a fleet's first send, which is
/// precisely when the protocol says to send — stay pinned by a test.
fn unknown_recipient_warning(known: &[agents::AgentInfo], to: &str) -> Option<String> {
    if agents::is_known(known, to) {
        return None;
    }
    // Each suggestion carries how long since that agent last did anything.
    // A correction is a claim about what you meant to type, and "alice-prime,
    // last seen 3h ago" is a claim the reader can judge where a bare name is
    // not. Names older than the suggestion horizon are not offered at all —
    // see agents::is_stale_for_suggestion.
    let hits: Vec<String> = agents::suggest(known, to)
        .into_iter()
        .map(|name| {
            // Only annotate an age worth judging. "tui-dev (last seen 0s ago)"
            // is noise on the common case — a peer that is working right now —
            // and noise is what stops the useful case being read.
            let stale = known
                .iter()
                .find(|a| a.name == name)
                .and_then(|a| age_of(&a.last_seen))
                .filter(|secs| *secs >= ANNOTATE_SUGGESTION_AGE_SECS);
            match stale {
                Some(secs) => format!("{name} (last seen {} ago)", human_secs(secs)),
                None => name,
            }
        })
        .collect();
    let did_you_mean = if hits.is_empty() {
        String::new()
    } else {
        format!(" — did you mean {}?", hits.join(", "))
    };
    Some(format!(
        "warning: no agent named {to:?} has acted in this repo \
         (no lease, no message sent){did_you_mean} (sending anyway)"
    ))
}

/// pact-rnc.7: the outbox. Same shape as the inbox with TO instead of FROM, and
/// the marker means something different and more useful: whether the *recipient*
/// has read it (pact-rnc.17's shared read state). An agent that cannot confirm a
/// send re-sends it — that is where the fleet's duplicate messages came from.
fn render_sent(messages: &[msg::Message]) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        String::new(), // unread marker; a header would be wider than the column
        "TO".to_string(),
        "SUBJECT".to_string(),
        "BODY".to_string(),
    ]];
    rows.extend(messages.iter().map(|m| {
        vec![
            m.id.clone(),
            if read_by_recipient(m) { " " } else { "*" }.to_string(),
            m.to.clone(),
            one_line(m.subject.as_deref().unwrap_or(""), 50),
            one_line(&m.body, 60),
        ]
    }));
    let unread = messages.iter().filter(|m| !read_by_recipient(m)).count();
    format!(
        "{}\n\n{} message(s), {unread} not read yet (*) by the recipient",
        table(&rows),
        messages.len(),
    )
}

/// `Message.read` is read-by-*me*, which is always true for something I sent.
/// The sender's question is whether the person they told has looked.
fn read_by_recipient(m: &msg::Message) -> bool {
    m.read_by.contains(&m.to)
}

/// pact-rnc.1 + pact-rnc.2: one line per message with the sender and an unread
/// marker. Seven messages used to print ~9KB of full bodies, which burned an
/// agent's context on every check and made `msg read` pointless.
///
/// `WHEN` (pact-m7j.12.3): named in both real fleet retrospectives and
/// `sut-analysis.md` as a missing at-a-glance signal — an agent deciding
/// whether an unread message is worth a context switch had to run `msg read`
/// just to see how stale it was. `Message.created_at` was already carried
/// end to end; this is `pact log`'s own `since()` reused for the same
/// column, not a new mechanism.
/// The inbox an agent actually reads: correspondence, with `pact watch` notices
/// summarised rather than listed (pact-mqw.5).
///
/// The default is authored-only because a notice is a side effect of a peer
/// doing its job, and the queue an agent checks for "does anybody need something
/// from me" must not be dominated by them. In the crucible run it was, 11 to 1,
/// and the one authored message in that window was a warning about six duplicate
/// test functions.
///
/// Notices are never *hidden*: the trailing line always counts them, per path,
/// and names the flag that shows them. A count an agent can see is what makes
/// skipping them a decision instead of an accident.
fn render_inbox_view(
    authored: &[&msg::Message],
    notices: &[msg::NoticeGroup],
    view: msg::WatchView,
    full: bool,
) -> String {
    // Nothing at all keeps the string it has always had: "inbox empty" is what
    // an agent's first command prints on a quiet repo, and "no authored
    // messages" would imply something else is waiting.
    if authored.is_empty() && notices.is_empty() {
        return "inbox empty".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if view != msg::WatchView::Only {
        if authored.is_empty() {
            parts.push("no authored messages".to_string());
        } else if full {
            parts.push(render_full(authored));
        } else {
            parts.push(render_inbox(authored));
        }
    }

    if notices.is_empty() {
        if view == msg::WatchView::Only {
            parts.push("no watch notices".to_string());
        }
    } else if view == msg::WatchView::Authored {
        // The summary, not the notices. One line, because its job is to let an
        // agent decide whether to look — not to be the looking.
        let total: usize = notices.iter().map(|g| g.count).sum();
        let unread: usize = notices.iter().map(|g| g.unread).sum();
        let per_path = notices
            .iter()
            .map(|g| format!("{} ×{}", g.path, g.count))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!(
            "{total} watch notice(s) on {} path(s), {unread} unread: {per_path}\n\
             `pact msg inbox --include-watch` lists them, `--watch-only` shows only them",
            notices.len(),
        ));
    } else {
        // One row per PATH, not per delivery. Nine diffs of one file nine
        // seconds apart answer one question and only the last of them answers
        // it, so the earlier ones are counted and the latest is the one offered
        // to `pact msg read`.
        let mut rows = vec![vec![
            "PATH".to_string(),
            "CHANGES".to_string(),
            "UNREAD".to_string(),
            "LATEST FROM".to_string(),
            "WHEN".to_string(),
            "LATEST ID".to_string(),
        ]];
        rows.extend(notices.iter().map(|g| {
            vec![
                g.path.clone(),
                g.count.to_string(),
                g.unread.to_string(),
                if g.latest_from.is_empty() {
                    "?".to_string()
                } else {
                    g.latest_from.clone()
                },
                since(&g.latest_at),
                g.latest_id.clone(),
            ]
        }));
        // "changed under you" is wrong for a reserved key: nothing changed, a lock
        // was let go (pact-bsf). Only claim a change when at least one group is a
        // real file, so a waiter watching only mutexes is not told its lock moved.
        let any_file = notices.iter().any(|g| !lease::is_mutex(&g.path));
        parts.push(format!(
            "{}\n\n{} path(s) {} — `pact msg read <latest id>` for the newest",
            table(&rows),
            notices.len(),
            if any_file {
                "changed under you"
            } else {
                "were released"
            },
        ));
    }
    parts.join("\n\n")
}

fn render_inbox(messages: &[&msg::Message]) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        String::new(), // unread marker; a header would be wider than the column
        "FROM".to_string(),
        "WHEN".to_string(),
        "SUBJECT".to_string(),
        "BODY".to_string(),
    ]];
    rows.extend(messages.iter().map(|m| {
        vec![
            m.id.clone(),
            if m.read { " " } else { "*" }.to_string(),
            if m.from.is_empty() { "?" } else { &m.from }.to_string(),
            since(&m.created_at),
            one_line(m.subject.as_deref().unwrap_or(""), 50),
            one_line(&m.body, 60),
        ]
    }));
    let unread = messages.iter().filter(|m| !m.read).count();
    format!(
        "{}\n\n{} message(s), {unread} unread (*) — `pact msg read <id>` for the full text",
        table(&rows),
        messages.len(),
    )
}

/// One stored message, however many recipients it fanned out to (pact-83r.8).
///
/// [`msg::Message`] is one copy PER RECIPIENT so `--json` keeps `to` a single
/// name, and that is not changing — a machine consumer is pinned to it. But a
/// human renderer that walks the fan-out prints the body once per recipient: a
/// 15-recipient broadcast cost ~280 KB to read, and it bit hardest on exactly the
/// messages that mattered, because the run that measured it REQUIRED hot-file
/// changers to broadcast to every dependent. Regrouping here collapses what the
/// API layer legitimately fans out.
///
/// Copies of one message are adjacent in every listing pact builds (fan-out is
/// per record), so this is a linear pass and not a sort.
fn group_by_id<'a>(messages: &[&'a msg::Message]) -> Vec<Vec<&'a msg::Message>> {
    let mut out: Vec<Vec<&msg::Message>> = Vec::new();
    for m in messages {
        match out.last_mut() {
            Some(g) if g[0].id == m.id => g.push(m),
            _ => out.push(vec![m]),
        }
    }
    out
}

/// The recipients, once, split by whether they have acknowledged it.
///
/// The union of the two lists is the recipient list, so nobody is named twice —
/// and "who still owes this a look" is the question a sender actually has, where
/// the old per-copy `(unread)` marker only ever answered it one recipient at a
/// time.
fn roster(group: &[&msg::Message]) -> String {
    let (read, unread): (Vec<&str>, Vec<&str>) = group
        .iter()
        .map(|m| m.to.as_str())
        .partition(|to| group[0].read_by.iter().any(|a| a == to));
    let mut parts = Vec::new();
    if !read.is_empty() {
        parts.push(format!("read by {}", read.join(", ")));
    }
    if !unread.is_empty() {
        parts.push(format!("unread by {}", unread.join(", ")));
    }
    parts.join(" — ")
}

/// Full text with the envelope pact used to throw away: from, to, subject, time
/// (pact-rnc.1). Shared by `msg read` and `msg inbox --full`, so a sender can
/// finally read their own message back with its metadata.
fn render_full(messages: &[&msg::Message]) -> String {
    group_by_id(messages)
        .iter()
        .map(|g| {
            let m = g[0];
            format!(
                "[{}] from: {}  to: {}\nsubject: {}\nat: {}  thread: {}\n\n{}",
                m.id,
                if m.from.is_empty() { "?" } else { &m.from },
                roster(g),
                m.subject.as_deref().unwrap_or("(none)"),
                m.created_at,
                m.thread,
                m.body,
            )
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

/// How much of a body `--brief` shows. Enough to tell a warning from a status
/// ping, which is the triage decision; the id to read it in full is on the line
/// above either way.
const BRIEF_LINES: usize = 5;

/// `--brief`: envelope, subject and the head of the body, for deciding which of
/// a thread's messages to read in full rather than reading all of them.
fn render_brief(messages: &[&msg::Message]) -> String {
    group_by_id(messages)
        .iter()
        .map(|g| {
            let m = g[0];
            let head: Vec<&str> = m.body.lines().take(BRIEF_LINES).collect();
            let rest = m.body.lines().count().saturating_sub(head.len());
            format!(
                "[{}] from: {}  to: {}\nsubject: {}\nat: {}\n\n{}{}",
                m.id,
                if m.from.is_empty() { "?" } else { &m.from },
                roster(g),
                m.subject.as_deref().unwrap_or("(none)"),
                m.created_at,
                head.join("\n"),
                if rest == 0 {
                    String::new()
                } else {
                    format!(
                        "\n… {rest} more line(s) — `pact msg read {}` for the full text",
                        m.id
                    )
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

#[cfg(test)]
mod tests {
    use super::super::message;
    use super::*;

    /// A `pact watch` release notice, subject-shaped the way
    /// `watch::notify_release` builds it so `split_notices` can parse the path
    /// back out.
    fn notice(id: &str, path: &str, holder: &str, read: bool) -> msg::Message {
        msg::Message {
            subject: Some(format!("{path}{}{holder}", msg::NOTICE_SUBJECT_MARKER)),
            from: holder.to_string(),
            notice: true,
            ..message(id, holder, "a diff", read)
        }
    }

    /// One stored message fanned out to N recipients, the shape `msg read`
    /// hands the renderer.
    fn broadcast(id: &str, body: &str, to: &[&str], read_by: &[&str]) -> Vec<msg::Message> {
        to.iter()
            .map(|t| msg::Message {
                to: t.to_string(),
                read: read_by.contains(t),
                read_by: read_by.iter().map(|a| a.to_string()).collect(),
                ..message(id, "msg-fix", body, false)
            })
            .collect()
    }

    fn agent_info(name: &str, leases: usize, sent: usize, received: usize) -> agents::AgentInfo {
        agents::AgentInfo {
            name: name.to_string(),
            // Now, not a fixed date: this fixture means "an agent that
            // exists", and a hardcoded stamp silently ages past the
            // suggestion horizon and changes what the test is asserting.
            last_seen: chrono::Utc::now().to_rfc3339(),
            leases_held: leases,
            lease_events: 0,
            messages_sent: sent,
            messages_received: received,
            name_valid: true,
            harness: None,
            model: None,
        }
    }

    /// pact-rnc.1 + pact-rnc.2: sender, unread marker, one line per message.
    #[test]
    fn inbox_shows_from_and_an_unread_marker_on_one_line_each() {
        let body = "para one\n\npara two with \"quotes\"\n".repeat(40);
        let a = message("pact-wisp-aaa", "msg-fix", &body, false);
        let b = message("pact-wisp-bbb", "lease-fix", "short", true);
        let out = render_inbox(&[&a, &b]);
        let rows: Vec<&str> = out.lines().take(3).collect();
        assert_eq!(rows.len(), 3, "header + one line per message: {out}");
        assert!(
            rows[1].contains("msg-fix") && rows[1].contains('*'),
            "{}",
            rows[1]
        );
        assert!(
            rows[2].contains("lease-fix") && !rows[2].contains('*'),
            "{}",
            rows[2]
        );
        assert!(
            rows[1].chars().count() < 200,
            "row must not be a wall: {}",
            rows[1]
        );
        assert!(out.contains("1 unread"), "{out}");
    }

    /// pact-mqw.5: the inbox an agent reads is correspondence, and watch notices
    /// are a trailing count. Reproduces the crucible ratio directly — 11
    /// automatic notices to 1 authored message — and asserts the authored one is
    /// the thing on screen.
    #[test]
    fn the_default_inbox_is_authored_only_with_notices_counted() {
        let mut all: Vec<msg::Message> = (0..9)
            .map(|i| notice("n{i}", "src/ast.rs", &format!("agent-0{i}"), false))
            .collect();
        all.push(notice("n9", "src/eval.rs", "agent-09", false));
        all.push(notice("n10", "src/eval.rs", "agent-09", true));
        let authored_msg = message("a1", "agent-05", "six duplicate test fns", false);
        all.push(authored_msg);

        let (authored, notices) = msg::split_notices(&all);
        let out = render_inbox_view(&authored, &notices, msg::WatchView::Authored, false);

        assert!(
            out.contains("agent-05"),
            "the authored message must show: {out}"
        );
        assert!(
            !out.contains("src/ast.rs ×9\nn0"),
            "notices must not be listed row by row: {out}"
        );
        // Counted, per path, with the flag that reveals them.
        assert!(out.contains("11 watch notice(s) on 2 path(s)"), "{out}");
        assert!(out.contains("src/ast.rs ×9"), "{out}");
        assert!(out.contains("src/eval.rs ×2"), "{out}");
        assert!(out.contains("10 unread"), "{out}");
        assert!(out.contains("--include-watch"), "{out}");
        // And the notice ids are NOT in the default table, or nothing was saved.
        assert!(!out.contains("n0"), "{out}");
    }

    /// `--watch-only` is one row per PATH, not per delivery. Nine diffs of one
    /// file nine seconds apart answer one question and only the last answers it,
    /// so the latest id is what gets offered to `msg read`.
    #[test]
    fn watch_only_coalesces_per_path_and_offers_the_newest_diff() {
        let all: Vec<msg::Message> = vec![
            notice("n0", "src/ast.rs", "agent-01", true),
            notice("n1", "src/ast.rs", "agent-04-r2", false),
            notice("n2", "src/printer.rs", "agent-02", false),
        ];
        let (authored, notices) = msg::split_notices(&all);
        let out = render_inbox_view(&authored, &notices, msg::WatchView::Only, false);

        let rows: Vec<&str> = out.lines().filter(|l| l.contains("src/")).collect();
        assert_eq!(rows.len(), 2, "one row per path: {out}");
        let ast = rows.iter().find(|r| r.contains("src/ast.rs")).unwrap();
        assert!(ast.contains("agent-04-r2"), "the latest releaser: {ast}");
        assert!(ast.contains("n1"), "the latest id: {ast}");
        assert!(
            !out.contains("n0"),
            "a superseded diff is a count, not a row: {out}"
        );
        // No authored section at all in this view.
        assert!(!out.contains("no authored messages"), "{out}");
    }

    /// An inbox with nothing but notices must not read as "inbox empty" — that
    /// was the shape that let a fleet believe nothing had happened.
    #[test]
    fn an_inbox_of_only_notices_says_so_rather_than_looking_empty() {
        let all = vec![notice("n0", "src/ast.rs", "agent-01", false)];
        let (authored, notices) = msg::split_notices(&all);
        let out = render_inbox_view(&authored, &notices, msg::WatchView::Authored, false);
        assert!(out.contains("no authored messages"), "{out}");
        assert!(out.contains("1 watch notice(s)"), "{out}");
    }

    #[test]
    fn full_render_carries_the_envelope() {
        let m = message("pact-wisp-aaa", "msg-fix", "the body", true);
        let out = render_full(&[&m]);
        assert!(out.contains("from: msg-fix"), "{out}");
        assert!(out.contains("to: read by cli-wire"), "{out}");
        assert!(out.contains("subject: a subject"), "{out}");
        assert!(out.contains("the body"), "{out}");
    }

    /// pact-83r.8: the renderer used to walk the per-recipient fan-out, so a
    /// 15-recipient broadcast printed the body 15 times — ~280 KB to read one
    /// message, and worst on the broadcasts that mattered most.
    #[test]
    fn a_broadcast_renders_its_body_once_and_its_recipients_once() {
        let to: Vec<String> = (0..15).map(|i| format!("agent-{i:02}")).collect();
        let names: Vec<&str> = to.iter().map(String::as_str).collect();
        let fanned = broadcast("pact-wisp-aaa", "MAX_QUADS moved", &names, &names[..2]);
        let out = render_full(&fanned.iter().collect::<Vec<_>>());

        assert_eq!(out.matches("MAX_QUADS moved").count(), 1, "{out}");
        // One envelope, not fifteen — the id still appears twice, as the
        // message's own and as its thread's.
        assert_eq!(out.matches("subject: a subject").count(), 1, "{out}");
        // Each recipient exactly once, split by who still owes it a look. The
        // union of the two lists IS the recipient list, so naming them plainly
        // as well would be the duplication this bead is about.
        for name in &names {
            assert_eq!(out.matches(name).count(), 1, "{name} twice in {out}");
        }
        assert!(
            out.contains("to: read by agent-00, agent-01 — unread by agent-02"),
            "{out}"
        );
    }

    /// Two distinct messages in one thread still render as two.
    #[test]
    fn grouping_collapses_recipients_not_messages() {
        let mut all = broadcast("pact-wisp-aaa", "the question", &["alpha", "bravo"], &[]);
        all.extend(broadcast("pact-wisp-bbb", "the answer", &["msg-fix"], &[]));
        let out = render_full(&all.iter().collect::<Vec<_>>());
        assert_eq!(out.matches("\n---\n").count(), 1, "{out}");
        assert!(
            out.contains("the question") && out.contains("the answer"),
            "{out}"
        );
    }

    #[test]
    fn brief_shows_the_head_of_the_body_and_how_to_get_the_rest() {
        let body = (0..12).map(|i| format!("line {i}\n")).collect::<String>();
        let fanned = broadcast("pact-wisp-aaa", &body, &["alpha"], &[]);
        let out = render_brief(&fanned.iter().collect::<Vec<_>>());
        assert!(out.contains("subject: a subject"), "{out}");
        assert!(out.contains("from: msg-fix"), "{out}");
        assert!(out.contains("line 4") && !out.contains("line 5"), "{out}");
        assert!(out.contains("… 7 more line(s)"), "{out}");
        assert!(out.contains("pact msg read pact-wisp-aaa"), "{out}");

        // A body that fits is shown whole, with no dangling "more lines" tail.
        let short = broadcast("pact-wisp-bbb", "one line", &["alpha"], &[]);
        let out = render_brief(&short.iter().collect::<Vec<_>>());
        assert!(
            out.contains("one line") && !out.contains("more line(s)"),
            "{out}"
        );
    }

    /// pact-rnc.5, the bead's own tui-dev/tuidev incident: the second send to a
    /// typo must warn exactly as loudly as the first, and the correction offered
    /// must be a name somebody answers to.
    #[test]
    fn a_typod_recipient_warns_every_time_not_once() {
        let known = [
            agent_info("tui-dev", 1, 0, 0),
            agent_info("tuidev", 0, 0, 2),
        ];

        let warning = unknown_recipient_warning(&known, "tuidev").expect("must still warn");
        assert!(warning.contains("did you mean tui-dev?"), "{warning}");
        assert!(unknown_recipient_warning(&known, "tui-dev").is_none());
        // The operator's mailbox is reserved by the protocol, not earned.
        assert!(unknown_recipient_warning(&known, agents::HUMAN).is_none());
    }

    /// The cold-start hole: `pact msg send` comes *before* `pact lease acquire`
    /// in the protocol, so the first sender legitimately has no trace — which is
    /// exactly when a typo used to ship silently.
    #[test]
    fn an_empty_registry_still_warns() {
        let warning = unknown_recipient_warning(&[], "alic").expect("cold start must warn");
        assert!(warning.contains("alic"), "{warning}");
        assert!(!warning.contains("did you mean"), "nobody to suggest yet");
    }

    /// A name pact's own grammar rejects can never be an agent, so warning about
    /// it is pointless: the message would be unreadable forever.
    #[test]
    fn an_impossible_recipient_is_refused_not_warned() {
        assert!(check_recipient("Not A Valid Agent").is_err());
        assert!(check_recipient("tui-dev").is_ok());
        assert!(check_recipient("human").is_ok());
    }

    /// pact-rnc.25: --body-file promises byte fidelity, so trailing blank lines
    /// inside a deliberately formatted body are content. Exactly one newline
    /// comes off — the one a file or heredoc ends with.
    #[test]
    fn body_file_strips_one_trailing_newline_not_all_whitespace() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("body.md");
        let table = "| a | b |\n|---|---|\n| 1 | 2 |\n\n\n";
        std::fs::write(&path, table).unwrap();

        let got = read_body(path.to_str().unwrap()).unwrap();
        assert_eq!(got, "| a | b |\n|---|---|\n| 1 | 2 |\n\n");
        // Trailing spaces are content too (indented code blocks).
        std::fs::write(&path, "x\n    \n").unwrap();
        assert_eq!(read_body(path.to_str().unwrap()).unwrap(), "x\n    ");
        // A body with no trailing newline at all is left alone.
        std::fs::write(&path, "no newline").unwrap();
        assert_eq!(read_body(path.to_str().unwrap()).unwrap(), "no newline");
        // And an all-whitespace body still has nothing in it to send: the
        // send path rejects on `body.trim().is_empty()`, which this satisfies.
        std::fs::write(&path, "\n\n  \n").unwrap();
        assert!(read_body(path.to_str().unwrap()).unwrap().trim().is_empty());
    }

    /// pact-rnc.7 + pact-rnc.17: the outbox exists so a sender can stop
    /// guessing. `read` is read-by-me and always true for my own sends, so the
    /// marker has to come from the recipient's own read-by label.
    #[test]
    fn sent_shows_the_recipient_and_whether_they_read_it() {
        let mut read_by_them = message("pact-wisp-aaa", "cli-wire", "the body", true);
        read_by_them.to = "lease-fix".to_string();
        read_by_them.read_by = vec!["lease-fix".to_string()];
        let mut unread_by_them = message("pact-wisp-bbb", "cli-wire", "the body", true);
        unread_by_them.to = "msg-fix".to_string();
        unread_by_them.read_by = vec!["cli-wire".to_string()];

        let out = render_sent(&[read_by_them, unread_by_them]);
        let rows: Vec<&str> = out.lines().take(3).collect();
        assert!(
            rows[1].contains("lease-fix") && !rows[1].contains('*'),
            "{out}"
        );
        assert!(
            rows[2].contains("msg-fix") && rows[2].contains('*'),
            "{out}"
        );
        assert!(out.contains("1 not read yet"), "{out}");
    }
}
