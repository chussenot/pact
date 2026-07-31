//! Advisory file leases: atomic lock files under `.pact/leases/`, with TTL,
//! steal, and re-entrant-refresh semantics. See docs/pact-scaffolding-prompt.md.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::output::exit_with;
use crate::repo::pact_dir;

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

impl LeaseEntry {
    /// What an operator actually needs to know: is this claim fresh, probably
    /// abandoned, or reclaimable? `remaining_secs` alone reads as "long-held"
    /// on a lease that is seconds old (pact-rnc.10).
    ///   "active"  — within its ttl
    ///   "stale"   — past its ttl but inside the GRACE_SECS clock-skew window,
    ///               i.e. probably abandoned but not yet reclaimable
    ///   "expired" — past ttl + GRACE_SECS; another agent may take it
    pub fn state(&self) -> &'static str {
        // Keyed off `expired` rather than recomputing, so the label can never
        // disagree with the GC decision made in `list`.
        if self.expired {
            "expired"
        } else if self.remaining_secs < 0 {
            "stale"
        } else {
            "active"
        }
    }

    /// The state as an operator reads it, including when a stale lease becomes
    /// reclaimable. Lives here, not in a renderer: `pact lease ls` and `pact ui`
    /// both show lease state, and having each format it its own way is what left
    /// the dashboard printing a raw `80s 3520s active` after pact-rnc.10 was
    /// "fixed" in the CLI. One implementation, both surfaces.
    pub fn state_label(&self) -> String {
        match self.state() {
            "stale" => format!(
                "stale (reclaimable in {})",
                human_secs(self.remaining_secs + GRACE_SECS)
            ),
            other => other.to_string(),
        }
    }
}

/// Compact duration: `45s`, `2m5s`, `1h3m`. Here for the same reason as
/// `state_label`: a bare four-digit second count next to an age is what made
/// pact-rnc.10 misreadable, so no renderer gets to reinvent it.
pub fn human_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// Encode a repo-root-relative path into a lock filename: `/` -> `__`.
/// Collision caveat: a path containing literal `__` can collide with a
/// different path whose separators encode to the same string. Acceptable in v1.
pub fn encode_path(relative_path: &str) -> String {
    relative_path.replace('/', "__")
}

/// Normalize `path` relative to `repo_root`: if absolute and under `repo_root`,
/// strip the prefix; otherwise assume it's already given relative to the repo
/// root. Deliberately simple — a lease can be acquired on a path that doesn't
/// exist yet, so we don't canonicalize against the filesystem.
fn normalize_path(repo_root: &Path, path: &str) -> String {
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        p.strip_prefix(repo_root).unwrap_or(p)
    } else {
        p
    };
    rel.to_string_lossy().into_owned()
}

fn lock_file_path(repo_root: &Path, relative: &str) -> Result<PathBuf> {
    let dir = pact_dir(repo_root)?;
    Ok(dir
        .join("leases")
        .join(format!("{}.lock", encode_path(relative))))
}

fn parse_acquired(lease: &LeaseInfo) -> DateTime<Utc> {
    // A lock file with an unparsable timestamp is a corruption case we don't
    // expect in practice (we always write RFC3339 ourselves); treat it as
    // "just now" so it reads as not-yet-expired rather than panicking.
    DateTime::parse_from_rfc3339(&lease.acquired_at)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn is_expired(lease: &LeaseInfo, now: DateTime<Utc>) -> bool {
    let acquired = parse_acquired(lease);
    now > acquired + chrono::Duration::seconds(lease.ttl_secs as i64 + GRACE_SECS)
}

fn age_and_remaining(lease: &LeaseInfo, now: DateTime<Utc>) -> (i64, i64) {
    let acquired = parse_acquired(lease);
    let age = (now - acquired).num_seconds();
    (age, lease.ttl_secs as i64 - age)
}

/// Write `lease` to `lock_path` atomically: write to a sibling temp file, then
/// rename over the destination (rename is atomic on the same filesystem).
fn write_lease_atomic(lock_path: &Path, lease: &LeaseInfo) -> Result<()> {
    let dir = lock_path
        .parent()
        .context("lock path unexpectedly has no parent")?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = dir.join(format!("tmp-{}-{nanos}", std::process::id()));
    let json = serde_json::to_string_pretty(lease)?;
    std::fs::write(&tmp_path, json).with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, lock_path)
        .with_context(|| format!("renaming into {}", lock_path.display()))?;
    Ok(())
}

fn read_lease(lock_path: &Path) -> Result<LeaseInfo> {
    let contents = std::fs::read_to_string(lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parsing lease at {}", lock_path.display()))
}

pub fn acquire(
    repo_root: &Path,
    agent: &str,
    path: &str,
    ttl_secs: u64,
    steal: bool,
    note: Option<String>,
) -> Result<AcquireOutcome> {
    let relative = normalize_path(repo_root, path);
    let lock_path = lock_file_path(repo_root, &relative)?;
    let now = Utc::now();
    let new_lease = LeaseInfo {
        agent: agent.to_string(),
        path: relative.clone(),
        acquired_at: now.to_rfc3339(),
        ttl_secs,
        note,
    };

    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(mut f) => {
            let json = serde_json::to_string_pretty(&new_lease)?;
            f.write_all(json.as_bytes())
                .with_context(|| format!("writing {}", lock_path.display()))?;
            Ok(AcquireOutcome {
                lease: new_lease,
                stolen: false,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_lease(&lock_path)?;

            if is_expired(&existing, now) {
                write_lease_atomic(&lock_path, &new_lease)?;
                Ok(AcquireOutcome {
                    lease: new_lease,
                    stolen: true,
                })
            } else if existing.agent == agent {
                // Re-entrant refresh: same holder, just bump acquired_at.
                write_lease_atomic(&lock_path, &new_lease)?;
                Ok(AcquireOutcome {
                    lease: new_lease,
                    stolen: false,
                })
            } else if steal {
                eprintln!(
                    "warning: stealing non-expired lease on {relative} held by {} (advisory override via --steal)",
                    existing.agent
                );
                write_lease_atomic(&lock_path, &new_lease)?;
                Ok(AcquireOutcome {
                    lease: new_lease,
                    stolen: true,
                })
            } else {
                let (age, remaining) = age_and_remaining(&existing, now);
                Err(exit_with(
                    2,
                    format!(
                        "lease on {relative} is held by {} ({age}s old, {remaining}s remaining); use --steal to override",
                        existing.agent
                    ),
                ))
            }
        }
        Err(e) => Err(e).with_context(|| format!("creating lock file {}", lock_path.display())),
    }
}

/// Release a lease. Returns `Some(displaced_agent)` when `force` destroyed a
/// *different* agent's live claim, so the caller can warn and name them the way
/// `acquire --steal` already does (pact-rnc.11); `None` when the caller held it
/// or nothing was held.
pub fn release(repo_root: &Path, agent: &str, path: &str, force: bool) -> Result<Option<String>> {
    let relative = normalize_path(repo_root, path);
    let lock_path = lock_file_path(repo_root, &relative)?;

    let existing = match read_lease(&lock_path) {
        Ok(lease) => lease,
        Err(_) if !lock_path.exists() => return Ok(None), // idempotent: nothing to release
        Err(e) => return Err(e),
    };

    let displaced = if existing.agent == agent {
        None
    } else if force {
        Some(existing.agent.clone())
    } else {
        return Err(exit_with(
            2,
            format!(
                "lease on {relative} is held by {}, not {agent} (use --force to override)",
                existing.agent
            ),
        ));
    };

    match std::fs::remove_file(&lock_path) {
        Ok(()) => Ok(displaced),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(displaced),
        Err(e) => Err(e).with_context(|| format!("removing {}", lock_path.display())),
    }
}

/// Release every lease held by `agent`, so "release everything I hold" is one
/// call that cannot be half-forgotten (pact-rnc.8). Returns the released paths,
/// sorted; holding nothing is success with an empty Vec.
pub fn release_all(repo_root: &Path, agent: &str) -> Result<Vec<String>> {
    let mut paths: Vec<String> = list(repo_root, true)?
        .into_iter()
        .filter(|e| e.lease.agent == agent)
        .map(|e| e.lease.path)
        .collect();
    paths.sort();
    for path in &paths {
        release(repo_root, agent, path, false)?;
    }
    Ok(paths)
}

/// Refresh `acquired_at` on a lease `agent` already holds, so a long task can
/// outlive its TTL on purpose instead of by accident (pact-rnc.9).
/// Deliberately does NOT create a missing lease: a typo'd path must not
/// silently claim something new.
pub fn renew(repo_root: &Path, agent: &str, path: &str) -> Result<LeaseInfo> {
    let relative = normalize_path(repo_root, path);
    let lock_path = lock_file_path(repo_root, &relative)?;

    if !lock_path.exists() {
        anyhow::bail!("no lease on {relative} to renew (use `pact lease acquire` to claim it)");
    }
    let existing = read_lease(&lock_path)?;
    if existing.agent != agent {
        return Err(exit_with(
            2,
            format!(
                "lease on {relative} is held by {}, not {agent}",
                existing.agent
            ),
        ));
    }

    let renewed = LeaseInfo {
        acquired_at: Utc::now().to_rfc3339(),
        ..existing
    };
    write_lease_atomic(&lock_path, &renewed)?;
    Ok(renewed)
}

/// List active leases, garbage-collecting expired ones as a side effect.
/// `all` includes expired leases in the returned list (still GC'd from disk).
pub fn list(repo_root: &Path, all: bool) -> Result<Vec<LeaseEntry>> {
    let leases_dir = pact_dir(repo_root)?.join("leases");
    let now = Utc::now();
    let mut entries = Vec::new();

    for entry in std::fs::read_dir(&leases_dir)
        .with_context(|| format!("reading {}", leases_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue;
        }
        // Advisory tooling, not a database: garbage on disk (partial writes,
        // hand-edited files) is skipped rather than treated as fatal.
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(lease) = serde_json::from_str::<LeaseInfo>(&contents) else {
            continue;
        };

        let (age_secs, remaining_secs) = age_and_remaining(&lease, now);
        let expired = is_expired(&lease, now);

        if expired {
            let _ = std::fs::remove_file(&path);
            if !all {
                continue;
            }
        }

        entries.push(LeaseEntry {
            lease,
            age_secs,
            remaining_secs,
            expired,
        });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn encode_path_replaces_slashes() {
        assert_eq!(encode_path("a/b/c"), "a__b__c");
        assert_eq!(encode_path("single"), "single");
    }

    fn lease_aged(ttl_secs: u64, age_secs: i64) -> (LeaseInfo, DateTime<Utc>) {
        let now = Utc::now();
        let acquired = now - Duration::seconds(age_secs);
        (
            LeaseInfo {
                agent: "agent-a".into(),
                path: "x".into(),
                acquired_at: acquired.to_rfc3339(),
                ttl_secs,
                note: None,
            },
            now,
        )
    }

    #[test]
    fn expiry_respects_grace_period_boundary() {
        let ttl = 100u64;
        let (lease, now) = lease_aged(ttl, ttl as i64 + GRACE_SECS - 1);
        assert!(
            !is_expired(&lease, now),
            "ttl+grace-1s should not be expired yet"
        );

        let (lease, now) = lease_aged(ttl, ttl as i64 + GRACE_SECS + 1);
        assert!(is_expired(&lease, now), "ttl+grace+1s should be expired");
    }

    /// Same boundary style as `expiry_respects_grace_period_boundary`, one age
    /// per state.
    #[test]
    fn state_labels_the_three_ttl_bands() {
        let ttl = 100u64;
        let entry_at = |age: i64| {
            let (lease, now) = lease_aged(ttl, age);
            let (age_secs, remaining_secs) = age_and_remaining(&lease, now);
            let expired = is_expired(&lease, now);
            LeaseEntry {
                lease,
                age_secs,
                remaining_secs,
                expired,
            }
        };

        assert_eq!(entry_at(1).state(), "active");
        assert_eq!(entry_at(ttl as i64 - 1).state(), "active");
        // Past ttl but inside the grace window: probably abandoned, not yet
        // reclaimable.
        assert_eq!(entry_at(ttl as i64 + 1).state(), "stale");
        assert_eq!(entry_at(ttl as i64 + GRACE_SECS - 1).state(), "stale");
        assert_eq!(entry_at(ttl as i64 + GRACE_SECS + 1).state(), "expired");

        // The label every renderer shows, so `pact ui` and `pact lease ls`
        // cannot drift apart again (pact-rnc.10).
        assert_eq!(entry_at(1).state_label(), "active");
        assert!(entry_at(ttl as i64 + 1)
            .state_label()
            .starts_with("stale (reclaimable in "));
        assert_eq!(
            entry_at(ttl as i64 + GRACE_SECS + 1).state_label(),
            "expired"
        );
    }

    #[test]
    fn human_secs_bands() {
        assert_eq!(human_secs(0), "0s");
        assert_eq!(human_secs(-5), "0s");
        assert_eq!(human_secs(59), "59s");
        assert_eq!(human_secs(125), "2m5s");
        assert_eq!(human_secs(3725), "1h2m");
    }

    fn repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn claim(root: &Path, agent: &str, path: &str) {
        acquire(root, agent, path, DEFAULT_TTL_SECS, false, None).unwrap();
    }

    fn held_by(root: &Path, agent: &str) -> Vec<String> {
        let mut paths: Vec<String> = list(root, true)
            .unwrap()
            .into_iter()
            .filter(|e| e.lease.agent == agent)
            .map(|e| e.lease.path)
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn release_all_releases_only_the_callers_leases() {
        let tmp = repo();
        let root = tmp.path();
        claim(root, "agent-a", "src/b.rs");
        claim(root, "agent-a", "src/a.rs");
        claim(root, "agent-b", "src/other.rs");

        assert_eq!(
            release_all(root, "agent-a").unwrap(),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
        assert!(held_by(root, "agent-a").is_empty());
        assert_eq!(held_by(root, "agent-b"), vec!["src/other.rs".to_string()]);
    }

    #[test]
    fn release_all_with_nothing_held_succeeds_empty() {
        let tmp = repo();
        claim(tmp.path(), "agent-b", "src/other.rs");
        assert!(release_all(tmp.path(), "agent-a").unwrap().is_empty());
        assert_eq!(
            held_by(tmp.path(), "agent-b"),
            vec!["src/other.rs".to_string()]
        );
    }

    #[test]
    fn release_reports_the_displaced_holder_only_when_forced() {
        let tmp = repo();
        let root = tmp.path();

        claim(root, "agent-a", "mine.rs");
        assert_eq!(release(root, "agent-a", "mine.rs", false).unwrap(), None);
        assert_eq!(release(root, "agent-a", "mine.rs", false).unwrap(), None); // idempotent

        claim(root, "agent-a", "theirs.rs");
        assert!(release(root, "agent-b", "theirs.rs", false).is_err());
        assert_eq!(
            release(root, "agent-b", "theirs.rs", true).unwrap(),
            Some("agent-a".to_string())
        );
    }

    #[test]
    fn renew_refreshes_acquired_at_for_the_holder() {
        let tmp = repo();
        let root = tmp.path();
        let first = acquire(root, "agent-a", "f.rs", 42, false, Some("note".into()))
            .unwrap()
            .lease;

        let renewed = renew(root, "agent-a", "f.rs").unwrap();
        assert_ne!(renewed.acquired_at, first.acquired_at);
        assert_eq!(renewed.ttl_secs, 42, "renew keeps the original ttl");
        assert_eq!(renewed.note.as_deref(), Some("note"));
        assert_eq!(
            read_lease(&lock_file_path(root, "f.rs").unwrap())
                .unwrap()
                .acquired_at,
            renewed.acquired_at,
            "renew persists to disk"
        );
    }

    #[test]
    fn renew_without_an_existing_lease_errors() {
        let tmp = repo();
        assert!(renew(tmp.path(), "agent-a", "typo.rs").is_err());
        assert!(
            !lock_file_path(tmp.path(), "typo.rs").unwrap().exists(),
            "renew must not create a lease"
        );
    }

    #[test]
    fn renew_of_another_agents_lease_exits_2() {
        let tmp = repo();
        claim(tmp.path(), "agent-a", "f.rs");
        let err = renew(tmp.path(), "agent-b", "f.rs").unwrap_err();
        assert_eq!(crate::output::code_for(&err), 2);
        assert_eq!(
            read_lease(&lock_file_path(tmp.path(), "f.rs").unwrap())
                .unwrap()
                .agent,
            "agent-a"
        );
    }
}
