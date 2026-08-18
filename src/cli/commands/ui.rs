//! `pact ui` — the dashboard, until the operator quits it.
//!
//! Owns one line of policy in three: identity is resolved best-effort and
//! discarded on failure. The dashboard is for a human watching a fleet, and
//! that human may never have set `PACT_AGENT` — refusing to open would deny
//! them the view precisely when they most need it.

use anyhow::Result;
use std::path::Path;

use crate::{identity, repo, tui};

/// `pact ui`: the dashboard, until the operator quits it.
pub(in crate::cli) fn run_ui(cwd: &Path, agent_flag: Option<&str>) -> Result<i32> {
    let root = repo::find_repo_root(cwd)?;
    let agent = identity::resolve_agent(agent_flag).ok();
    tui::run(root, agent).map(|()| 0)
}
