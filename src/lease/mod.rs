//! Advisory file leases: atomic lock files under `.pact/leases/`, with TTL,
//! steal, and re-entrant-refresh semantics. See docs/leases.md.

mod context_stamp;
mod lifecycle;
mod release;
mod store;
mod types;

use context_stamp::*;
pub use lifecycle::*;
pub use release::*;
pub use store::*;
pub use types::*;

/// Fixtures shared by every submodule's tests: build a repo, claim a path,
/// age a claim, read back what is held. They live here rather than in any one
/// sibling because no sibling owns them — a release test needs to acquire and
/// an acquire test needs to inspect.
#[cfg(test)]
mod testutil {
    use super::*;
    use crate::events;
    use chrono::Duration;
    use chrono::{DateTime, Utc};
    use std::collections::BTreeMap;
    use std::path::Path;

    pub(super) fn lease_aged(ttl_secs: u64, age_secs: i64) -> (LeaseInfo, DateTime<Utc>) {
        let now = Utc::now();
        let acquired = now - Duration::seconds(age_secs);
        (
            LeaseInfo {
                agent: "agent-a".into(),
                path: "x".into(),
                acquired_at: acquired.to_rfc3339(),
                ttl_secs,
                note: None,
                branch: None,
                worktree: None,
                invoked_from: None,
                content_hash: None,
                harness: None,
                model: None,
                extra: BTreeMap::new(),
            },
            now,
        )
    }

    pub(super) fn repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    pub(super) fn claim(root: &Path, agent: &str, path: &str) {
        acquire(root, agent, path, DEFAULT_TTL_SECS, false, None).unwrap();
    }

    pub(super) fn held_by(root: &Path, agent: &str) -> Vec<String> {
        let mut paths: Vec<String> = list_reclaiming(root, true)
            .unwrap()
            .0
            .into_iter()
            .filter(|e| e.lease.agent == agent)
            .map(|e| e.lease.path)
            .collect();
        paths.sort();
        paths
    }

    /// Plant a lock file that is already `age_secs` old, without going through
    /// `acquire` — the only way to test expiry without sleeping.
    pub(super) fn claim_aged(root: &Path, agent: &str, path: &str, ttl_secs: u64, age_secs: i64) {
        claim_at(
            root,
            agent,
            path,
            ttl_secs,
            Utc::now() - Duration::seconds(age_secs),
        );
    }

    /// Same as `claim_aged`, but takes the exact `acquired_at` instant rather
    /// than an age relative to the real wall clock — needed to plant a lease
    /// relative to a fabricated clock watermark instead of real "now".
    /// An `Event` with every optional field empty, so a test can name only the
    /// three or four fields it is actually about. `Event` has no `Default` on
    /// purpose — every production writer should have to think about each field —
    /// but a test planting a fixture is not that.
    pub(super) fn blank_event() -> events::Event {
        events::Event {
            at: String::new(),
            agent: String::new(),
            kind: String::new(),
            path: None,
            detail: None,
            ttl_secs: None,
            covers_lines: None,
            actor: None,
            displaced: None,
            chain_hash: None,
            invoked_from: None,
            collected_from: None,
            scope: None,
            pact_version: None,
            content_hash: None,
            subscriber: None,
            message_id: None,
            protocol_hash: None,
            head: None,
            holder: None,
            holder_remaining_secs: None,
            holder_branch: None,
            holder_worktree: None,
            ..Default::default()
        }
    }

    pub(super) fn claim_at(
        root: &Path,
        agent: &str,
        path: &str,
        ttl_secs: u64,
        acquired_at: DateTime<Utc>,
    ) {
        let lease = LeaseInfo {
            agent: agent.into(),
            path: path.into(),
            acquired_at: acquired_at.to_rfc3339(),
            ttl_secs,
            note: None,
            branch: None,
            worktree: None,
            invoked_from: None,
            content_hash: None,
            harness: None,
            model: None,
            extra: BTreeMap::new(),
        };
        write_lease_atomic(&lock_file_path(root, path).unwrap(), &lease).unwrap();
    }

    pub(super) fn lock_exists(root: &Path, path: &str) -> bool {
        lock_file_path(root, path).unwrap().exists()
    }

    // ---- pact-rnc.19: peek() answers without mutating -------------------

    pub(super) fn event_kinds(root: &Path) -> Vec<(String, String)> {
        crate::events::recent(root, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.path.unwrap_or_default()))
            .collect()
    }
}
