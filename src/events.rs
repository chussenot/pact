//! Lease event log: an append-only JSONL feed at `.pact/events.jsonl`.
//!
//! Why this file exists at all (pact-rnc.13): `lease ls` shows only the
//! instantaneous set, and releasing a lease *deletes the only record of it*,
//! so a lease taken and dropped while you looked away leaves no trace. Lease
//! history therefore cannot be derived — it has to be logged.
//!
//! Deliberately kept as small as new persisted state can be:
//!   * ONE file, `.pact/events.jsonl`, already gitignored by the `.pact/` rule.
//!   * LEASE events only. Message events are derivable from bd and are NOT
//!     duplicated here — two sources of truth for one fact is worse than none.
//!   * [`append`] cannot fail the caller. A missing feed is an inconvenience;
//!     a lease acquire that failed because logging failed is a coordination bug.
//!   * Bounded: see [`MAX_LINES`].
//!   * Garbage lines are skipped, not fatal, exactly as `lease::list` skips
//!     unparsable lock files.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::repo::pact_dir;

/// Bounded growth. An unbounded log in a long-lived repo is a slow leak, and
/// nobody reads the 20000th-most-recent lease acquire. Strategy, kept dumb on
/// purpose: after an append, if the file exceeds `MAX_LINES`, rewrite it with
/// only the newest `KEEP_LINES`. No rotation, no sidecar files, no index. The
/// slack between the two constants is what stops every append past the cap from
/// rewriting the file; as written it rewrites once per `MAX_LINES - KEEP_LINES`
/// appends. At ~150 bytes a line the file stays under a megabyte.
const MAX_LINES: usize = 5000;
const KEEP_LINES: usize = 4000;

/// One lease-lifecycle event. `kind` is one of the strings emitted by
/// `lease.rs`: `"acquired"`, `"renewed"`, `"released"`, `"stolen"`,
/// `"force-released"`, `"expired"`. Kept as a plain `String` rather than an enum
/// so an older `pact` reading a newer log shows an unknown kind instead of
/// refusing to parse the line.
///
/// `"expired"` is the only kind whose `agent` did not run the command that wrote
/// it: a lapsed lease is noticed by whoever collects the lock, and the event
/// belongs to the holder whose claim ended (pact-rnc.13).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// RFC3339.
    pub at: String,
    pub agent: String,
    pub kind: String,
    /// The leased path, for lease events.
    pub path: Option<String>,
    /// Free text: the lease note, the displaced holder, etc.
    pub detail: Option<String>,
}

fn events_file(repo_root: &Path) -> Result<PathBuf> {
    Ok(pact_dir(repo_root)?.join("events.jsonl"))
}

/// Append one event to `.pact/events.jsonl`.
///
/// Infallible by signature: I/O errors are swallowed, because a logging
/// failure must never break the lease operation that triggered it.
pub fn append(repo_root: &Path, ev: &Event) {
    let _ = append_bounded(repo_root, ev, MAX_LINES, KEEP_LINES);
}

/// The fallible body of [`append`], with the cap injected so tests don't have
/// to write 5000 lines to exercise trimming.
fn append_bounded(repo_root: &Path, ev: &Event, max_lines: usize, keep_lines: usize) -> Result<()> {
    let path = events_file(repo_root)?;
    let line = serde_json::to_string(ev)?;

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(f, "{line}").with_context(|| format!("appending to {}", path.display()))?;
    drop(f);

    // Reading the file back on every append is a few hundred microseconds at
    // this cap, and lease operations happen at agent speed, not in a loop.
    let contents = std::fs::read_to_string(&path)?;
    if contents.lines().count() > max_lines {
        let kept: Vec<&str> = contents
            .lines()
            .skip(contents.lines().count().saturating_sub(keep_lines))
            .collect();
        // Rewrite via temp + rename so a reader never sees a half-trimmed file.
        // ponytail: an append racing the rename is lost with the old inode.
        // Acceptable for an advisory feed; needs a lock file if it ever isn't.
        let tmp = path.with_extension(format!("jsonl.tmp-{}", std::process::id()));
        std::fs::write(&tmp, kept.join("\n") + "\n")?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// The most recent lease events, oldest-first (so a feed reads top-to-bottom
/// like a log), at most `limit`. A missing file is an empty feed, not an error.
/// Unparsable lines are skipped.
pub fn recent(repo_root: &Path, limit: usize) -> Result<Vec<Event>> {
    let path = events_file(repo_root)?;
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let events: Vec<Event> = contents
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let start = events.len().saturating_sub(limit);
    Ok(events[start..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, path: &str) -> Event {
        Event {
            at: chrono::Utc::now().to_rfc3339(),
            agent: "agent-a".into(),
            kind: kind.into(),
            path: Some(path.into()),
            detail: None,
        }
    }

    #[test]
    fn append_then_recent_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &ev("acquired", "src/a.rs"));

        let got = recent(tmp.path(), 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "acquired");
        assert_eq!(got[0].path.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn recent_on_a_missing_feed_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(recent(tmp.path(), 10).unwrap().is_empty());
    }

    #[test]
    fn recent_returns_the_newest_limit_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5 {
            append(tmp.path(), &ev("acquired", &format!("f{i}.rs")));
        }

        let paths: Vec<String> = recent(tmp.path(), 3)
            .unwrap()
            .into_iter()
            .map(|e| e.path.unwrap())
            .collect();
        assert_eq!(
            paths,
            vec!["f2.rs", "f3.rs", "f4.rs"],
            "newest 3, oldest first"
        );
        assert_eq!(recent(tmp.path(), 0).unwrap().len(), 0);
        assert_eq!(
            recent(tmp.path(), 99).unwrap().len(),
            5,
            "limit above len is fine"
        );
    }

    #[test]
    fn a_corrupt_line_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &ev("acquired", "good1.rs"));
        // A partial write or a hand-edit: half a line, then valid JSON that
        // isn't an Event, then a blank line.
        let file = events_file(tmp.path()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(f, "{{\"at\":\"2026-07-31T00:00").unwrap();
        writeln!(f, "{{\"unrelated\":true}}").unwrap();
        writeln!(f).unwrap();
        drop(f);
        append(tmp.path(), &ev("released", "good2.rs"));

        let paths: Vec<String> = recent(tmp.path(), 10)
            .unwrap()
            .into_iter()
            .map(|e| e.path.unwrap())
            .collect();
        assert_eq!(paths, vec!["good1.rs", "good2.rs"]);
    }

    #[test]
    fn append_on_an_unwritable_repo_root_is_silent() {
        // repo_root is a *file*, so `.pact/` can't be created.
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = tmp.path().join("regular-file");
        std::fs::write(&not_a_dir, "x").unwrap();

        append(&not_a_dir, &ev("acquired", "src/a.rs")); // must not panic
        assert!(
            recent(&not_a_dir, 10).is_err(),
            "reading it still reports why"
        );
    }

    #[test]
    fn trimming_caps_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..12 {
            append_bounded(tmp.path(), &ev("acquired", &format!("f{i}.rs")), 10, 6).unwrap();
        }

        let lines = std::fs::read_to_string(events_file(tmp.path()).unwrap())
            .unwrap()
            .lines()
            .count();
        assert!(lines <= 10, "file stays under the cap, got {lines} lines");
        // Trimming keeps the newest, and the file is still parseable after it.
        let got = recent(tmp.path(), 100).unwrap();
        assert_eq!(got.last().unwrap().path.as_deref(), Some("f11.rs"));
        assert_eq!(got.len(), lines);
    }
}
