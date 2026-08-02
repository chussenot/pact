use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::output::exit_with;

/// Walk up from `start` looking for a `.git` entry, returning the containing directory.
/// Shared by `lease`, `agents_md`, and `doctor` — kept separate from `main.rs` so
/// `main.rs` stays dispatch-only per the project layout.
pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut dir = start
        .canonicalize()
        .with_context(|| format!("resolving {}", start.display()))?;
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(exit_with(
                4,
                format!(
                    "not in a git repository (no .git found above {})",
                    start.display()
                ),
            ));
        }
    }
}

/// `.pact/` at the repo root, creating it (and `leases/`) if absent.
pub fn pact_dir(repo_root: &Path) -> Result<PathBuf> {
    let dir = repo_root.join(".pact");
    std::fs::create_dir_all(dir.join("leases"))
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Where `.pact/` would be, without creating anything. Read-only callers use
/// this: asking a question must not leave a directory behind (pact-rnc.27, and
/// the same principle as the non-mutating `lease::peek` in pact-rnc.19).
/// Callers must treat a missing directory as "no state yet", not an error.
pub fn pact_dir_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".pact")
}

/// Whether a file `pact init` writes will actually reach someone who clones
/// the repo.
///
/// `init`'s whole promise is "run it once, commit the result, every clone is
/// onboarded". A gitignored `AGENTS.md` breaks that silently and completely:
/// the file is there locally, `git add` refuses it without a word, and the
/// clone gets no protocol. It happened in pact's own repo — a global
/// `~/.gitignore` rule meant `AGENTS.md` was never committed once.
///
/// Answering this needs real gitignore semantics — global excludes, nested
/// ignore files, negations, precedence — so it asks `git` instead of
/// reimplementing them. This is the only place pact shells out to git: the rest
/// of the tool needs a `.git` directory, not the binary, so an absent or
/// failing `git` yields [`Reach::Unknown`] and `doctor` declines to guess.
pub enum Reach {
    /// Tracked, so a clone gets it.
    Tracked,
    /// Untracked *and* ignored — the silent failure.
    Ignored { source: String },
    /// Untracked but committable: normal before the first commit.
    Untracked,
    /// No usable `git` to ask.
    Unknown,
}

pub fn reach(repo_root: &Path, rel: &str) -> Reach {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(args)
            .output()
    };

    match git(&["ls-files", "--error-unmatch", "--", rel]) {
        Ok(o) if o.status.success() => return Reach::Tracked,
        // Exit 1 is "not tracked"; 128 is "not a repo". Both fall through to
        // check-ignore, which distinguishes them.
        Ok(_) => {}
        Err(_) => return Reach::Unknown,
    }

    match git(&["check-ignore", "-v", "--", rel]) {
        // `-v` prints "<source>:<line>:<pattern>\t<path>"; the source is the
        // actionable half — knowing *which* file ignores it is the fix.
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let source = stdout
                .split('\t')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            Reach::Ignored { source }
        }
        Ok(o) if o.status.code() == Some(1) => Reach::Untracked,
        _ => Reach::Unknown,
    }
}

/// What [`commit_paths`] did, so `init` can report it truthfully. Every
/// non-committed outcome is a *reported* one: silently not committing is the
/// exact failure mode that let `AGENTS.md` go uncommitted for a whole project.
pub enum CommitOutcome {
    Committed {
        sha: String,
        files: Vec<String>,
    },
    /// Files already match HEAD — a re-run of `pact init` with nothing to say.
    NothingToCommit,
    /// Could not commit; the files are still written. Carries the reason.
    Skipped(String),
}

/// Stage and commit exactly `paths`, and nothing else.
///
/// Scoped on purpose: `git commit -- <paths>` builds the commit from HEAD plus
/// those paths, so a user's unrelated staged work stays staged instead of being
/// swept into a commit pact authored. Committing is a side effect a tool should
/// keep as small as it can.
///
/// Never uses `git add -f`: a path the repo ignores is a decision pact does not
/// get to overrule, so that case is reported and left alone.
pub fn commit_paths(repo_root: &Path, paths: &[&str], message: &str) -> CommitOutcome {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(args)
            .output()
    };

    let present: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| repo_root.join(p).exists())
        .collect();
    if present.is_empty() {
        return CommitOutcome::NothingToCommit;
    }

    if let Some(ignored) = present.iter().find_map(|p| match reach(repo_root, p) {
        Reach::Ignored { source } => Some(format!("{p} is ignored by {source}")),
        _ => None,
    }) {
        return CommitOutcome::Skipped(format!(
            "{ignored} — un-ignore it and commit by hand, pact will not override a repo's ignore rules"
        ));
    }

    let mut add = vec!["add", "--"];
    add.extend_from_slice(&present);
    match git(&add) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return CommitOutcome::Skipped(format!(
                "`git add` failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ))
        }
        Err(e) => return CommitOutcome::Skipped(format!("cannot run git: {e}")),
    }

    let mut staged = vec!["diff", "--cached", "--name-only", "--"];
    staged.extend_from_slice(&present);
    let changed: Vec<String> = match git(&staged) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => return CommitOutcome::Skipped("could not ask git what changed".to_string()),
    };
    if changed.is_empty() {
        return CommitOutcome::NothingToCommit;
    }

    let mut commit = vec!["commit", "--quiet", "-m", message, "--"];
    commit.extend_from_slice(&present);
    match git(&commit) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            // Hooks, an unset user.email, a signing key that is not there. The
            // files are written either way, so this is reported, not raised.
            let why = String::from_utf8_lossy(&o.stderr);
            let why = why.trim();
            let why = if why.is_empty() {
                String::from_utf8_lossy(&o.stdout).trim().to_string()
            } else {
                why.to_string()
            };
            return CommitOutcome::Skipped(format!("`git commit` failed: {why}"));
        }
        Err(e) => return CommitOutcome::Skipped(format!("cannot run git: {e}")),
    }

    let sha = match git(&["rev-parse", "--short", "HEAD"]) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "HEAD".to_string(),
    };
    CommitOutcome::Committed {
        sha,
        files: changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_root_from_nested_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let found = find_repo_root(&nested).unwrap();
        assert_eq!(found, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn errors_outside_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_repo_root(tmp.path()).is_err());
    }
}
