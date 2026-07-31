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
//! happens client-side. There's no read/unread lifecycle for messages, so
//! read state is tracked locally in `.pact/read.json`, keyed by agent.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::beads::BeadsCli;
use crate::repo::pact_dir;

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
    pub read: bool,
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
}

impl BdIssue {
    /// The one BdIssue -> Message mapping, shared by every read path so `from`
    /// cannot go missing on just one of them. `thread` pins the thread id
    /// (read_thread pins every row to the root); otherwise it is the parent,
    /// falling back to the message's own id for a thread root.
    fn into_message(self, thread: Option<&str>, read: bool) -> Message {
        Message {
            thread: thread
                .map(str::to_string)
                .or(self.parent)
                .unwrap_or_else(|| self.id.clone()),
            from: self.created_by.unwrap_or_default(),
            to: self.assignee.unwrap_or_default(),
            subject: Some(self.title),
            body: self.description.unwrap_or_default(),
            created_at: self.created_at,
            id: self.id,
            read,
        }
    }
}

pub fn send(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    to: &str,
    thread: Option<&str>,
    subject: Option<&str>,
    body: &str,
) -> Result<Message> {
    let title = subject
        .map(str::to_string)
        .unwrap_or_else(|| default_subject(body));
    let title_arg = format!("--title={title}");
    let desc_arg = format!("--description={body}");
    let assignee_arg = format!("--assignee={to}");
    // Records who (in pact's own identity scheme) sent this, in bd's audit trail.
    let actor_arg = format!("--actor={agent}");
    let parent_arg = thread.map(|t| format!("--parent={t}"));

    let mut args: Vec<&str> = vec![
        "create",
        "--type=message",
        "--json",
        &title_arg,
        &desc_arg,
        &assignee_arg,
        &actor_arg,
    ];
    if let Some(p) = &parent_arg {
        args.push(p);
    }

    let stdout = cli.run(repo_root, &args)?;
    let issue: BdIssue =
        serde_json::from_str(&stdout).context("parsing `bd create --json` output")?;
    let thread_id = thread
        .map(str::to_string)
        .unwrap_or_else(|| issue.id.clone());

    Ok(Message {
        id: issue.id,
        thread: thread_id,
        // The calling agent, not bd's echo: send() always passes --actor=agent.
        from: agent.to_string(),
        to: to.to_string(),
        subject: Some(title),
        body: body.to_string(),
        created_at: issue.created_at,
        read: false,
    })
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
    let mut messages = parse_messages(&stdout, &ReadState::load(repo_root)?)?;

    if unread_only {
        messages.retain(|m| !m.read);
    }
    messages.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(messages)
}

/// The root message plus its direct replies, oldest first. Marks everything
/// shown as read for `agent`.
pub fn read_thread(
    cli: &BeadsCli,
    repo_root: &Path,
    agent: &str,
    id: &str,
) -> Result<Vec<Message>> {
    let show_out = cli.run(repo_root, &["show", id, "--json"])?;
    let mut roots: Vec<BdIssue> =
        serde_json::from_str(&show_out).context("parsing `bd show --json` output")?;
    let root = roots
        .pop()
        .ok_or_else(|| anyhow::anyhow!("message {id} not found"))?;

    let parent_arg = format!("--parent={id}");
    let list_out = cli.run(
        repo_root,
        &["list", "--include-infra", "--json", &parent_arg],
    )?;
    let replies: Vec<BdIssue> =
        serde_json::from_str(&list_out).context("parsing `bd list --json` output")?;

    let mut all = vec![root];
    all.extend(replies.into_iter().filter(|i| i.issue_type == "message"));
    all.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let mut read_state = ReadState::load(repo_root)?;
    let messages: Vec<Message> = all
        .into_iter()
        .map(|i| {
            read_state.mark_read(agent, &i.id);
            i.into_message(Some(id), true)
        })
        .collect();
    read_state.save(repo_root)?;

    Ok(messages)
}

/// Every message bead in the repo, regardless of recipient, oldest first.
///
/// `bd list` hides message beads unless `--include-infra` is passed and has no
/// `--type` filter, so `issue_type == "message"` is filtered client-side, same
/// as `inbox()`. `read` is resolved per recipient, since read state is
/// per-agent and each message only has one.
pub fn all_messages(cli: &BeadsCli, repo_root: &Path) -> Result<Vec<Message>> {
    let stdout = cli.run(repo_root, &["list", "--include-infra", "--json"])?;
    parse_messages(&stdout, &ReadState::load(repo_root)?)
}

/// `bd list --json` output -> message beads only, oldest first. `read` is keyed
/// on each message's own recipient, which for `inbox()` is the requesting agent
/// (bd already filtered by `--assignee`).
fn parse_messages(stdout: &str, read_state: &ReadState) -> Result<Vec<Message>> {
    let issues: Vec<BdIssue> =
        serde_json::from_str(stdout).context("parsing `bd list --json` output")?;
    let mut messages: Vec<Message> = issues
        .into_iter()
        .filter(|i| i.issue_type == "message")
        .map(|i| {
            let read = read_state.is_read(i.assignee.as_deref().unwrap_or_default(), &i.id);
            i.into_message(None, read)
        })
        .collect();
    messages.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(messages)
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

/// Per-agent read-message ids, local runtime state (never committed — see
/// `ensure_ignored`). bd has no read/unread lifecycle for message beads, so
/// this is pact's own bookkeeping.
#[derive(Default, Serialize, Deserialize)]
struct ReadState(HashMap<String, HashSet<String>>);

impl ReadState {
    fn path(repo_root: &Path) -> Result<PathBuf> {
        Ok(pact_dir(repo_root)?.join("read.json"))
    }

    fn load(repo_root: &Path) -> Result<Self> {
        let path = Self::path(repo_root)?;
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(serde_json::from_str(&s).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn is_read(&self, agent: &str, id: &str) -> bool {
        self.0.get(agent).is_some_and(|ids| ids.contains(id))
    }

    fn mark_read(&mut self, agent: &str, id: &str) {
        self.0
            .entry(agent.to_string())
            .or_default()
            .insert(id.to_string());
    }

    fn save(&self, repo_root: &Path) -> Result<()> {
        let path = Self::path(repo_root)?;
        std::fs::write(&path, serde_json::to_string_pretty(&self.0)?)
            .with_context(|| format!("writing {}", path.display()))?;
        ensure_ignored(repo_root)
    }
}

/// `.pact/read.json` is local runtime state, same as leases — keep it out of
/// git. Deliberately self-contained rather than reusing `agents_md`'s
/// `.gitignore` helper, but a `.pact/` line written by that helper (pact-rnc.16)
/// already covers this file, so don't add a redundant rule under it.
fn ensure_ignored(repo_root: &Path) -> Result<()> {
    let path = repo_root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    if existing
        .lines()
        .any(|l| matches!(l.trim(), ".pact/read.json" | ".pact/"))
    {
        return Ok(());
    }

    let mut new_content = existing;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(".pact/read.json\n");
    std::fs::write(&path, new_content).with_context(|| format!("writing {}", path.display()))
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

    /// Shape copied from real `bd list --include-infra --json` output.
    const LIST_JSON: &str = r#"[
      {"id":"pact-wisp-1","title":"hi","description":"body one",
       "assignee":"msg-fix","created_by":"tui-dev","created_at":"2026-07-31T07:20:00Z",
       "issue_type":"message"},
      {"id":"pact-wisp-2","title":"re: hi","description":"body two",
       "assignee":"msg-fix","created_by":"Clement HUSSENOT-DESENONGES",
       "created_at":"2026-07-31T07:10:00Z","issue_type":"message","parent":"pact-wisp-1"},
      {"id":"pact-wisp-3","title":"anon","assignee":"msg-fix",
       "created_at":"2026-07-31T07:30:00Z","issue_type":"message"},
      {"id":"pact-rnc.1","title":"a real bug, not a message",
       "created_at":"2026-07-31T07:00:00Z","issue_type":"bug","created_by":"someone"}
    ]"#;

    #[test]
    fn parse_messages_keeps_from_and_drops_non_messages() {
        let msgs = parse_messages(LIST_JSON, &ReadState::default()).unwrap();

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

    #[test]
    fn parse_messages_resolves_read_per_recipient() {
        let mut state = ReadState::default();
        state.mark_read("msg-fix", "pact-wisp-1");
        state.mark_read("someone-else", "pact-wisp-3");

        let read: Vec<bool> = parse_messages(LIST_JSON, &state)
            .unwrap()
            .iter()
            .map(|m| m.read)
            .collect();
        // Only the one read by its own recipient counts.
        assert_eq!(read, [false, true, false]);
    }

    #[test]
    fn read_thread_mapping_pins_thread_to_root() {
        let issue: BdIssue = serde_json::from_str(
            r#"{"id":"pact-wisp-9","title":"deep reply","assignee":"human",
                "created_by":"lease-fix","created_at":"2026-07-31T08:00:00Z",
                "issue_type":"message","parent":"pact-wisp-8"}"#,
        )
        .unwrap();
        let m = issue.into_message(Some("pact-wisp-root"), true);
        assert_eq!(m.thread, "pact-wisp-root");
        assert_eq!(m.from, "lease-fix");
        assert_eq!(m.to, "human");
        assert!(m.read);
    }

    #[test]
    fn read_state_round_trips_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        let mut state = ReadState::default();
        assert!(!state.is_read("agent-a", "msg-1"));
        state.mark_read("agent-a", "msg-1");
        state.save(tmp.path()).unwrap();
        state.save(tmp.path()).unwrap(); // idempotent: no duplicate gitignore lines

        let reloaded = ReadState::load(tmp.path()).unwrap();
        assert!(reloaded.is_read("agent-a", "msg-1"));
        assert!(!reloaded.is_read("agent-b", "msg-1"));

        let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(gitignore.matches(".pact/read.json").count(), 1);
    }

    /// pact-rnc.16: agents_md now writes a single `.pact/` line, which already
    /// covers read.json — don't append a redundant rule under it.
    #[test]
    fn read_state_respects_a_broad_pact_gitignore_line() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "/target\n.pact/\n").unwrap();

        ReadState::default().save(tmp.path()).unwrap();

        let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(gitignore, "/target\n.pact/\n");
    }
}
