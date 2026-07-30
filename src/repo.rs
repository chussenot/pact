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
                format!("not in a git repository (no .git found above {})", start.display()),
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
