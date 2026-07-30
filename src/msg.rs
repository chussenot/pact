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
    created_at: String,
    #[serde(default)]
    issue_type: String,
    #[serde(default)]
    parent: Option<String>,
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
    let issues: Vec<BdIssue> =
        serde_json::from_str(&stdout).context("parsing `bd list --json` output")?;

    let read_state = ReadState::load(repo_root)?;
    let mut messages: Vec<Message> = issues
        .into_iter()
        .filter(|i| i.issue_type == "message")
        .map(|i| {
            let read = read_state.is_read(agent, &i.id);
            Message {
                thread: i.parent.clone().unwrap_or_else(|| i.id.clone()),
                subject: Some(i.title),
                body: i.description.unwrap_or_default(),
                created_at: i.created_at,
                to: agent.to_string(),
                id: i.id,
                read,
            }
        })
        .collect();

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
    let thread_id = id.to_string();
    let messages: Vec<Message> = all
        .into_iter()
        .map(|i| {
            read_state.mark_read(agent, &i.id);
            Message {
                thread: thread_id.clone(),
                to: i.assignee.unwrap_or_default(),
                subject: Some(i.title),
                body: i.description.unwrap_or_default(),
                created_at: i.created_at,
                id: i.id,
                read: true,
            }
        })
        .collect();
    read_state.save(repo_root)?;

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
/// `.gitignore` helper, since that one only manages the `.pact/leases/` line.
fn ensure_ignored(repo_root: &Path) -> Result<()> {
    let path = repo_root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    if existing.lines().any(|l| l.trim() == ".pact/read.json") {
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
}
