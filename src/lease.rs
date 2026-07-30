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

pub fn release(repo_root: &Path, agent: &str, path: &str, force: bool) -> Result<()> {
    let relative = normalize_path(repo_root, path);
    let lock_path = lock_file_path(repo_root, &relative)?;

    let existing = match read_lease(&lock_path) {
        Ok(lease) => lease,
        Err(_) if !lock_path.exists() => return Ok(()), // idempotent: nothing to release
        Err(e) => return Err(e),
    };

    if existing.agent != agent && !force {
        return Err(exit_with(
            2,
            format!(
                "lease on {relative} is held by {}, not {agent} (use --force to override)",
                existing.agent
            ),
        ));
    }

    match std::fs::remove_file(&lock_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", lock_path.display())),
    }
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
}
