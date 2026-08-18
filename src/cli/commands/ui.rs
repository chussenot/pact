use anyhow::Result;
use std::path::Path;

use crate::{identity, repo, tui};

/// `pact ui`: the dashboard, until the operator quits it.
pub(in crate::cli) fn run_ui(cwd: &Path, agent_flag: Option<&str>) -> Result<i32> {
    let root = repo::find_repo_root(cwd)?;
    let agent = identity::resolve_agent(agent_flag).ok();
    tui::run(root, agent).map(|()| 0)
}
