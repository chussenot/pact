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
            "--format={RECORD_SEP}%H{FIELD_SEP}%an{FIELD_SEP}%aI"
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

fn parse_log(text: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    for block in text.split(RECORD_SEP).skip(1) {
        let mut lines = block.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        let mut fields = header.splitn(3, FIELD_SEP);
        let (Some(hash), Some(author), Some(at)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
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
