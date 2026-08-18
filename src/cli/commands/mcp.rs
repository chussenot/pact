//! `pact mcp serve` — the read-only observation surface, on stdio.
//!
//! Owns the absence of an identity. An observer holds nothing and sends
//! nothing, so there is no agent for it to be; the tools that need one take it
//! as a parameter, because a single observer may watch several agents.

use anyhow::Result;
use std::path::Path;

use crate::cli::McpAction;
use crate::{mcp, repo};

/// `pact mcp serve`: the read-only observation surface, on stdio.
///
/// No identity resolved and none needed: an observer holds nothing and
/// sends nothing, so there is no agent for it to be. The tools that need
/// one take it as a parameter, because an observer may watch several.
pub(in crate::cli) fn run_mcp(cwd: &Path, action: McpAction) -> Result<i32> {
    match action {
        McpAction::Serve => mcp::serve(repo::find_repo_root(cwd)?),
    }
}
