//! Idempotent managed section in `AGENTS.md` teaching agents the pact
//! coordination protocol. Never touches content outside the markers.

use std::path::Path;

use anyhow::Result;

pub const BEGIN_MARKER: &str = "<!-- pact:begin -->";
pub const END_MARKER: &str = "<!-- pact:end -->";

/// The protocol block injected between the markers.
pub fn managed_block() -> String {
    todo!("~20 line coordination protocol: identity, inbox, lease, release, --json")
}

/// Idempotently write the managed block into `AGENTS.md` at the repo root
/// (creating the file if absent). Running twice produces zero diff.
pub fn apply(_repo_root: &Path) -> Result<std::path::PathBuf> {
    todo!("splice managed_block() between markers, preserving everything else byte-for-byte")
}

/// Add a single `.pact/leases/` line to `.gitignore`, idempotently, only if missing.
pub fn ensure_gitignore(_repo_root: &Path) -> Result<()> {
    todo!("append .pact/leases/ to .gitignore if not already present")
}
