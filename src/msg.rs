//! Threaded messaging, layered on `bd create --type=message` +
//! `--parent`/`--assignee`/`--include-infra` (see docs/pact-scaffolding-prompt.md
//! and the design notes on bd-4xh.4 for how these flags were confirmed).

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::beads::BeadsCli;

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

pub fn send(
    _cli: &BeadsCli,
    _repo_root: &Path,
    _agent: &str,
    _to: &str,
    _thread: Option<&str>,
    _subject: Option<&str>,
    _body: &str,
) -> Result<Message> {
    todo!("bd create --type=message --assignee=<to> [--parent=<thread>] --json")
}

pub fn inbox(
    _cli: &BeadsCli,
    _repo_root: &Path,
    _agent: &str,
    _unread_only: bool,
) -> Result<Vec<Message>> {
    todo!("bd list --assignee=<agent> --include-infra --json, filtered to issue_type==message")
}

pub fn read_thread(
    _cli: &BeadsCli,
    _repo_root: &Path,
    _agent: &str,
    _id: &str,
) -> Result<Vec<Message>> {
    todo!("bd show <id> --json + bd list --parent <id> --include-infra --json, merged by created_at")
}
