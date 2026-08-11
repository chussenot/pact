//! Git commit history, read the same way `repo::reach` already shells out to
//! `git` (`std::process::Command`, `-C <root>`) — no `git2`/libgit2
//! dependency. `git` on `PATH` is already a hard requirement (pact only ever
//! runs inside a git repository) and pact's established pattern is to shell
//! out to a CLI it depends on rather than link its library — see `beads.rs`
//! for the same choice about `bd`/`br`.
//!
//! Used by `audit::Check::CommitCorrelation` (pact-1l8.1) to ask whether real
//! commits landed inside the lease windows `.pact/events.jsonl` records.
//!
//! ## Known gaps, on purpose
//!
//! - **Merge commits carry no file list.** `git log --name-only` reports no
//!   changed paths for an ordinary merge commit, so a merge that actually
//!   brought in changes to a leased path is invisible here. Adding `-m` would
//!   fix that at the cost of listing every parent's diff for every merge,
//!   which is a much bigger `git log` for a case audit does not need to be
//!   exact about — findings degrade to "no commit seen", the same shape as a
//!   read-only lease, never a false positive.
//! - **A path rename is two identities.** `git log` with no `--follow` (this
//!   module never passes it) reports the OLD name before a rename and the NEW
//!   name after. A lease taken under one name only correlates against commits
//!   recorded under that same name.
//! - **A shallow clone sees only what it has.** `git log --since=...` on a
//!   shallow clone silently stops at the fetch boundary rather than erroring,
//!   so a commit older than the clone's depth reads as "no commit", not as a
//!   different kind of unknown.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// One commit, restricted to the paths it actually changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub author: String,
    pub at: DateTime<Utc>,
    pub paths: Vec<String>,
    /// The `Pact-Agent:` trailer, when the commit carries one (pact-mqw.10).
    ///
    /// `author` cannot answer "which agent made this": under every fleet topology
    /// so far, every agent commits under the same git identity, so a rogue working
    /// without a lease is invisible whenever a compliant peer happens to hold the
    /// path. This is the one field that can tell them apart, and it is `None` for
    /// every commit made before anyone started writing it — which is most of them,
    /// and why the check that reads it degrades rather than fails.
    pub pact_agent: Option<String>,
}

/// Neither byte is legal in a commit hash, an author name's typical shape, an
/// RFC3339 timestamp, or a file path — safe delimiters for a single-pass
/// `git log` whose output is otherwise free text.
const RECORD_SEP: char = '\u{1e}';
const FIELD_SEP: char = '\u{1f}';

/// Every commit at or after `since` (the whole history, if `None`), oldest
/// first, together with the paths it touched.
///
/// One `git log` invocation regardless of how many paths the caller cares
/// about — the commit-correlation check asks about every path any lease ever
/// touched, and one process beats one per path.
pub fn commits_since(repo_root: &Path, since: Option<DateTime<Utc>>) -> Result<Vec<Commit>> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(repo_root)
        .arg("log")
        .arg("--name-only")
        .arg(format!(
            // `%(trailers:key=Pact-Agent,valueonly)` rather than grepping the
            // body: git owns the trailer grammar (continuation lines, the
            // separator, where the trailer block starts), and reimplementing it
            // here would disagree with `git interpret-trailers` on exactly the
            // commits somebody hand-edited.
            "--format={RECORD_SEP}%H{FIELD_SEP}%an{FIELD_SEP}%aI{FIELD_SEP}%(trailers:key=Pact-Agent,valueonly,separator=%x2C)"
        ));
    if let Some(since) = since {
        cmd.arg(format!("--since={}", since.to_rfc3339()));
    }
    let output = cmd
        .output()
        .context("running `git log` to correlate leases with commits")?;
    if !output.status.success() {
        // A brand-new repo with zero commits exits non-zero here ("does not
        // have any commits yet") — that is "no commits", not a failure the
        // caller should degrade on the same way a missing `git` binary is.
        return Ok(Vec::new());
    }
    let mut commits = parse_log(&String::from_utf8_lossy(&output.stdout));
    // `git log` is newest-first by default; every caller wants to walk lease
    // windows chronologically, so this is the one place that ordering is
    // fixed rather than trusting `--reverse` (which changes other things
    // about traversal order on a history with merges) to do it.
    commits.sort_by_key(|c| c.at);
    Ok(commits)
}

/// The git blob id of each path's **working-tree** content, for the paths that
/// exist (pact-8qu).
///
/// `-w` writes the blob into the object database rather than only computing
/// its id, and that is the load-bearing part: `pact watch` diffs at release
/// against the content as it was at acquire, and the protocol now tells agents
/// to commit before releasing — so by release time the working tree is usually
/// clean and the at-acquire content exists nowhere else. An unreferenced loose
/// blob is a few bytes that `git gc` prunes; losing the diff is permanent.
///
/// Best-effort by signature: a missing `git`, a bare repo, an unreadable file
/// — every failure yields no entry for that path, and the caller treats a
/// missing hash as "cannot diff this one" rather than as an error. A lease
/// must never fail because a diff could not be prepared.
///
/// Paths that do not exist are simply absent from the result: a lease on a
/// file you are about to create is a documented workflow, and `git
/// hash-object` errors on the whole invocation if any argument is missing.
pub fn hash_objects(repo_root: &Path, paths: &[String]) -> BTreeMap<String, String> {
    let existing: Vec<&String> = paths
        .iter()
        .filter(|p| repo_root.join(p).is_file())
        .collect();
    if existing.is_empty() {
        return BTreeMap::new();
    }
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(repo_root)
        .args(["hash-object", "-w", "--"]);
    for p in &existing {
        cmd.arg(p);
    }
    let Ok(out) = cmd.output() else {
        return BTreeMap::new();
    };
    if !out.status.success() {
        return BTreeMap::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hashes: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    // One hash per input, in order. A mismatch means git answered something
    // this code does not understand, and pairing them up anyway would
    // attribute one file's content to another — worse than no diff at all.
    if hashes.len() != existing.len() {
        return BTreeMap::new();
    }
    existing
        .into_iter()
        .zip(hashes)
        .map(|(p, h)| (p.clone(), h.to_string()))
        .collect()
}

/// A unified diff between two blobs, or `None` when git cannot produce one.
///
/// Blob-to-blob rather than against `HEAD` or the index, for the same reason
/// [`hash_objects`] writes the object: the holder has usually committed by the
/// time they release, so every working-tree-relative diff would be empty. The
/// two blobs are fixed points that survive that.
pub fn diff_blobs(repo_root: &Path, old: &str, new: &str, path: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["diff", "--no-color", old, new])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    (!text.trim().is_empty()).then(|| relabel(&text, old, new, path))
}

/// Put the real filename back into a blob-to-blob diff's headers.
///
/// `git diff <oid> <oid>` has no filename to report, so it prints the object
/// ids: `--- a/22aa3231…`. Correct and unreadable. Only the three header lines
/// are rewritten, and only where the id appears verbatim, so nothing inside
/// the hunk bodies can be touched — a diff that happened to contain the id as
/// content keeps it. The result reads like an ordinary diff of the file, which
/// is what the subscriber needs (and is applicable with `git apply`).
fn relabel(diff: &str, old: &str, new: &str, path: &str) -> String {
    diff.lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("diff --git ") {
                if rest == format!("a/{old} b/{new}") {
                    return format!("diff --git a/{path} b/{path}");
                }
            }
            if line == format!("--- a/{old}") {
                return format!("--- a/{path}");
            }
            if line == format!("+++ b/{new}") {
                return format!("+++ b/{path}");
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The short hash of `HEAD`, so a truncated diff can point at something the
/// reader can go and look at in full.
pub fn head_short(repo_root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_log(text: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    for block in text.split(RECORD_SEP).skip(1) {
        let mut lines = block.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        let mut fields = header.splitn(4, FIELD_SEP);
        let (Some(hash), Some(author), Some(at)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // Absent, or present and empty, both mean "this commit says nothing about
        // which agent made it". A commit with two Pact-Agent trailers is also
        // "cannot tell" rather than a guess at which one is authoritative.
        let pact_agent = fields
            .next()
            .map(str::trim)
            .filter(|t| !t.is_empty() && !t.contains(','))
            .map(str::to_string);
        let Some(at) = DateTime::parse_from_rfc3339(at)
            .ok()
            .map(|d| d.with_timezone(&Utc))
        else {
            continue;
        };
        let paths: Vec<String> = lines
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        commits.push(Commit {
            hash: hash.to_string(),
            author: author.to_string(),
            at,
            paths,
            pact_agent,
        });
    }
    commits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "tester")
                .env("GIT_AUTHOR_EMAIL", "tester@example.com")
                .env("GIT_COMMITTER_NAME", "tester")
                .env("GIT_COMMITTER_EMAIL", "tester@example.com")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet"]);
        tmp
    }

    fn commit(repo: &Path, file: &str, contents: &str, at: &str) {
        std::fs::write(repo.join(file), contents).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", file])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["commit", "--quiet", "-m", &format!("touch {file}")])
            .env("GIT_AUTHOR_NAME", "tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_AUTHOR_DATE", at)
            .env("GIT_COMMITTER_NAME", "tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_DATE", at)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_brand_new_repo_with_zero_commits_is_empty_not_an_error() {
        let tmp = git_repo();
        assert_eq!(commits_since(tmp.path(), None).unwrap(), Vec::new());
    }

    #[test]
    fn commits_are_read_with_their_paths_oldest_first() {
        let tmp = git_repo();
        commit(tmp.path(), "a.rs", "one", "2026-08-01T10:00:00+00:00");
        commit(tmp.path(), "b.rs", "two", "2026-08-01T11:00:00+00:00");

        let commits = commits_since(tmp.path(), None).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].paths, ["a.rs"]);
        assert_eq!(commits[1].paths, ["b.rs"]);
        assert!(commits[0].at < commits[1].at);
        assert_eq!(commits[0].author, "tester");
        assert_eq!(commits[0].hash.len(), 40);
    }

    #[test]
    fn since_excludes_commits_strictly_before_it() {
        let tmp = git_repo();
        commit(tmp.path(), "old.rs", "one", "2026-08-01T00:00:00+00:00");
        commit(tmp.path(), "new.rs", "two", "2026-08-05T00:00:00+00:00");

        let since = DateTime::parse_from_rfc3339("2026-08-03T00:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let commits = commits_since(tmp.path(), Some(since)).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].paths, ["new.rs"]);
    }

    #[test]
    fn a_missing_git_binary_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        // No git repo here at all — `git -C <dir> log` fails to find one, and
        // that failure must come back as `Err`, not a panic, so the caller
        // can decide how to degrade.
        let result = commits_since(tmp.path(), None);
        assert!(result.is_err() || result.unwrap().is_empty());
    }
}
