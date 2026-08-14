//! Git commit history, read the same way `repo::reach` already shells out to
//! `git` (`std::process::Command`, `-C <root>`) — no `git2`/libgit2
//! dependency. `git` on `PATH` is already a hard requirement (pact only ever
//! runs inside a git repository) and pact's established pattern is to shell
//! out to a CLI it depends on rather than link its library — see `beads.rs`
//! for the same choice about `bd`.
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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
/// Which paths were touched by the commits in `from..to`, or `None` if that range
/// cannot be resolved here.
///
/// `None` is the important half of the return type and is NOT an error: a recorded
/// hash can legitimately stop resolving. A worktree branch gets deleted and its
/// objects garbage-collected, a branch is force-pushed, a fleet's run is analysed in a
/// shallow clone. The caller falls back to the timestamp window in every one of those
/// cases, so this reports "cannot answer" rather than failing the check.
///
/// Exclusive of `from` and inclusive of `to`, which is git's own `A..B` and exactly the
/// right shape for a hold: HEAD at acquire time is the commit the agent STARTED from,
/// so it is somebody else's work, and HEAD at release time is the agent's last.
pub fn commits_in_range(repo_root: &Path, from: &str, to: &str) -> Option<Vec<String>> {
    // Same hash means no commits, and asking git would be a wasted spawn on the
    // commonest case — a hold that landed nothing.
    if from == to {
        return Some(Vec::new());
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "log",
            "--name-only",
            "--pretty=format:",
            &format!("{from}..{to}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        // Either hash is unknown here: an unresolvable range, not an empty one.
        return None;
    }
    let mut paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    paths.sort_unstable();
    paths.dedup();
    Some(paths)
}

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
    // Cached per (process, repo). See `HEAD_CACHE` for why that is sound, and for
    // the one caller that has to opt out.
    if let Some(hit) = cached_head(repo_root) {
        return hit;
    }
    let fresh = head_short_uncached(repo_root);
    remember_head(repo_root, fresh.clone());
    fresh
}

/// What `head_short` used to be, and still is on a cache miss.
fn head_short_uncached(repo_root: &Path) -> Option<String> {
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

/// Resolved HEADs, keyed by repository root (pact-hxy).
///
/// ## Why this exists
///
/// `events::append` stamps `head` on the four hold-boundary kinds, so every
/// `acquired` and `released` row costs a `git rev-parse`. Measured, that one
/// subprocess is **85% of an append on the lease hot path**: the same append with
/// a non-boundary kind is 444 us against 2.95 ms
/// ([the numbers](../docs/performance.md)).
///
/// It buys nothing for a single-path `lease acquire`, which writes one event and
/// therefore spawns once either way. It buys a lot for the batching the protocol
/// actually asks agents to do: `lease acquire a b c` writes three `acquired` rows
/// with a byte-identical head, and `release --all` does the same on the way out.
/// At twenty paths that was twenty identical subprocesses.
///
/// ## Why caching cannot record a stale HEAD
///
/// Within one pact command HEAD cannot move: pact never commits, and no command
/// both writes boundary events and waits for something that might. So the value
/// is fixed for exactly as long as it is reused.
///
/// The exception is `pact ui`, which is the one long-lived process here and can
/// force-release from its own event loop. It calls [`forget_heads`] on every data
/// refresh, so its staleness window is one refresh tick rather than the lifetime
/// of the session — and a human's force-release happens moments after a refresh
/// they were looking at.
///
/// Keyed by root rather than global, because the test suite runs many
/// repositories in one process and a global slot would hand one tempdir's HEAD to
/// another's assertions. `None` is cached too: "this is not a git repo with a
/// commit" is an answer worth not re-deriving twenty times.
static HEAD_CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();

fn head_cache() -> &'static Mutex<HashMap<PathBuf, Option<String>>> {
    HEAD_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `Some(answer)` on a hit — where `answer` may itself be `None`. A poisoned lock
/// reads as a miss: recomputing is always correct, so a panic in another thread
/// must not turn this into an error path.
fn cached_head(repo_root: &Path) -> Option<Option<String>> {
    head_cache().lock().ok()?.get(repo_root).cloned()
}

fn remember_head(repo_root: &Path, head: Option<String>) {
    if let Ok(mut map) = head_cache().lock() {
        map.insert(repo_root.to_path_buf(), head);
    }
}

/// Drop the cached HEAD for ONE repository.
///
/// For long-lived processes: a command that exits has nothing to invalidate, and a
/// process that stays up must not keep answering with a commit that has since
/// moved. `pact ui` calls this on each refresh, for the repository it is watching.
///
/// Scoped to one root rather than clearing the map, and that is not tidiness. A
/// global clear reaches every OTHER repository in the process, which in a test
/// binary means every other test: the first version cleared everything, and a
/// `pact ui` test constructing an `App` wiped this module's own cache assertions
/// mid-test from another thread. Nothing needs a wider invalidation than the repo
/// it is looking at, so nothing gets one.
#[cfg_attr(not(feature = "ui"), allow(dead_code))]
pub fn forget_head(repo_root: &Path) {
    if let Ok(mut map) = head_cache().lock() {
        map.remove(repo_root);
    }
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
    /// pact-hxy: the cache's three properties, each of which is a way it could
    /// have been wrong.
    ///
    /// The measured win is on batching — `lease acquire a b c` wrote three
    /// `acquired` rows with a byte-identical head and spawned `git rev-parse`
    /// three times — so what matters is that reuse is *correct*, not that it is
    /// fast, which is what these assert.
    #[test]
    fn head_is_resolved_once_per_repo_and_forgotten_on_demand() {
        let a = git_repo();
        commit(a.path(), "one.txt", "1", "2026-08-13T09:00:00+00:00");
        let first = head_short(a.path()).expect("a HEAD");

        // 1. A hit returns the same answer.
        assert_eq!(head_short(a.path()).as_deref(), Some(first.as_str()));

        // 2. Keyed by repo, not global. A second repository must not be handed the
        //    first one's HEAD — the failure mode that would have quietly corrupted
        //    the test suite, where many repos share one process.
        let b = git_repo();
        commit(b.path(), "two.txt", "2", "2026-08-13T09:01:00+00:00");
        let b_head = head_short(b.path()).expect("b HEAD");
        assert_ne!(
            b_head, first,
            "two repos with different commits must not share a cached HEAD"
        );
        assert_eq!(head_short(a.path()).as_deref(), Some(first.as_str()));

        // 3. `forget_heads` actually re-resolves. Committing again moves HEAD, and
        //    the cache must not keep answering with the old one — this is the
        //    property `pact ui` depends on.
        commit(a.path(), "three.txt", "3", "2026-08-13T09:02:00+00:00");
        assert_eq!(
            head_short(a.path()).as_deref(),
            Some(first.as_str()),
            "still cached until told otherwise"
        );
        forget_head(a.path());
        let moved = head_short(a.path()).expect("a HEAD after the second commit");
        assert_ne!(moved, first, "forget_heads must force a re-resolve");
    }

    /// A repo with no commits caches its `None` rather than re-spawning `git` for
    /// the same answer — the shape a fresh `git init` has, and the one a bench
    /// against a tempdir hits constantly.
    #[test]
    fn a_repo_with_no_commits_caches_the_absence() {
        let tmp = git_repo();
        assert_eq!(head_short(tmp.path()), None);
        assert_eq!(
            cached_head(tmp.path()),
            Some(None),
            "the miss is remembered"
        );
    }
}
