//! `pact merge` — the mutex around an integration.
//!
//! Thin on purpose: the interesting part is [`crate::merge`]. What this file
//! owns is that `--ttl` is parsed by [`crate::lease::parse_ttl`] and fails with
//! the same exit 5 as everywhere else — one TTL grammar across the whole tool,
//! rather than a second one that accepts almost the same strings.

use anyhow::Result;
use std::path::Path;

use crate::cli::USAGE_ERROR;
use crate::{identity, lease, merge, output, repo};

/// `pact merge` — see [`crate::merge`] for why this is a command rather than
/// five lines of protocol prose.
pub(in crate::cli) fn run_merge(
    cwd: &Path,
    agent_flag: Option<&str>,
    json: bool,
    branch: &str,
    verify: Option<&str>,
    ttl: &str,
    allow_dirty: bool,
) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    let agent = identity::resolve_agent(agent_flag)?;
    // Same grammar and the same exit 5 as `lease acquire --ttl`: one TTL syntax
    // across the tool, so an agent that learned it once has learned it.
    let ttl = lease::parse_ttl(ttl).map_err(|e| output::exit_with(USAGE_ERROR, e.to_string()))?;

    let outcome = merge::merge(&root, &agent, branch, verify, ttl, allow_dirty)?;
    output::emit(json, &outcome, merge::describe);
    merge::warn_if_unproven(&outcome);
    Ok(())
}
