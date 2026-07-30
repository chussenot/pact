//! Advisory file leases: atomic lock files under `.pact/leases/`, with TTL,
//! steal, and re-entrant-refresh semantics. See docs/pact-scaffolding-prompt.md.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub const DEFAULT_TTL_SECS: u64 = 900;
/// Clock-skew tolerance: a lease is only considered expired past `ttl + GRACE_SECS`.
pub const GRACE_SECS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseInfo {
    pub agent: String,
    pub path: String,
    pub acquired_at: String, // RFC3339
    pub ttl_secs: u64,
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AcquireOutcome {
    pub lease: LeaseInfo,
    pub stolen: bool,
}

#[derive(Debug, Serialize)]
pub struct LeaseEntry {
    pub lease: LeaseInfo,
    pub age_secs: i64,
    pub remaining_secs: i64,
    pub expired: bool,
}

/// Encode a repo-root-relative path into a lock filename: `/` -> `__`.
/// Collision caveat: a path containing literal `__` can collide with a
/// different path whose separators encode to the same string. Acceptable in v1.
pub fn encode_path(relative_path: &str) -> String {
    relative_path.replace('/', "__")
}

pub fn acquire(
    _repo_root: &Path,
    _agent: &str,
    _path: &str,
    _ttl_secs: u64,
    _steal: bool,
    _note: Option<String>,
) -> Result<AcquireOutcome> {
    todo!("lease acquire: atomic create_new, expiry/steal, re-entrant refresh")
}

pub fn release(_repo_root: &Path, _agent: &str, _path: &str, _force: bool) -> Result<()> {
    todo!("lease release: holder-only, idempotent on missing lease, --force override")
}

/// List active leases, garbage-collecting expired ones as a side effect.
/// `all` includes expired leases in the returned list (still GC'd from disk).
pub fn list(_repo_root: &Path, _all: bool) -> Result<Vec<LeaseEntry>> {
    todo!("lease ls: read .pact/leases/*.lock, compute age/remaining, GC expired")
}
