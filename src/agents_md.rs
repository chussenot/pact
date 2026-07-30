//! Idempotent managed section in `AGENTS.md` teaching agents the pact
//! coordination protocol. Never touches content outside the markers.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const BEGIN_MARKER: &str = "<!-- pact:begin -->";
pub const END_MARKER: &str = "<!-- pact:end -->";

/// The protocol block injected between the markers.
pub fn managed_block() -> String {
    r#"## pact coordination protocol

pact coordinates multiple coding agents working in this repository. Follow
this protocol whenever you touch shared files or hand off work to others.

- **Identity**: your agent identity comes from the `PACT_AGENT` environment
  variable (or `--agent <name>` on any pact command). Set one before running
  pact commands; it will never guess or generate an identity for you.
- **Check your inbox first**: run `pact msg inbox` at the start of every task
  to see messages other agents have sent you.
- **Lease before you edit**: run `pact lease acquire <path>` before editing a
  file another agent might also be working on. Leases are advisory, not
  enforced by the filesystem — respect them anyway.
- **Release when you're done**: run `pact lease release <path>` as soon as
  you finish, so the file is free for the next agent.
- **Announce interface changes**: if you change an API, schema, CLI flag, or
  any other contract another agent might depend on, send them a message:
  `pact msg send --to <agent> "what changed and why"`.
- **Everything is scriptable**: every pact command accepts `--json` for
  machine-readable output; prefer it over parsing human-formatted text.

Run `pact doctor` if anything above seems out of date.
"#
    .to_string()
}

/// Idempotently write the managed block into `AGENTS.md` at the repo root
/// (creating the file if absent). Running twice produces zero diff.
pub fn apply(repo_root: &Path) -> Result<PathBuf> {
    let path = repo_root.join("AGENTS.md");
    let existing = read_or_empty(&path)?;

    let block = format!("{BEGIN_MARKER}\n{}{END_MARKER}\n", managed_block());

    let new_content = match find_block_bounds(&existing) {
        // Both markers present, in order: splice the new block in between
        // them, byte-for-byte identical everywhere else.
        Some((begin, end)) => {
            let mut s = String::with_capacity(existing.len() + block.len());
            s.push_str(&existing[..begin]);
            s.push_str(&block);
            s.push_str(&existing[end..]);
            s
        }
        // No markers (or only one — malformed, treated the same as "no
        // valid block" rather than trying to repair it): append a fresh
        // block after whatever is already there.
        None => {
            let mut s = existing;
            if !s.is_empty() {
                if !s.ends_with('\n') {
                    s.push('\n');
                }
                if !s.ends_with("\n\n") {
                    s.push('\n');
                }
            }
            s.push_str(&block);
            s
        }
    };

    std::fs::write(&path, new_content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Add a single `.pact/leases/` line to `.gitignore`, idempotently, only if missing.
pub fn ensure_gitignore(repo_root: &Path) -> Result<()> {
    let path = repo_root.join(".gitignore");
    let existing = read_or_empty(&path)?;

    let already_present = existing
        .lines()
        .any(|l| l.trim().trim_end_matches('/') == ".pact/leases");
    if already_present {
        return Ok(());
    }

    let mut new_content = existing;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(".pact/leases/\n");

    std::fs::write(&path, new_content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Whether AGENTS.md exists, has a managed block, and that block matches the
/// current `managed_block()` exactly (used by `pact doctor`).
pub fn is_current(repo_root: &Path) -> Result<bool> {
    let path = repo_root.join("AGENTS.md");
    let existing = read_or_empty(&path)?;
    let Some((begin, end)) = find_block_bounds(&existing) else {
        return Ok(false);
    };
    let expected = format!("{BEGIN_MARKER}\n{}{END_MARKER}\n", managed_block());
    Ok(existing[begin..end] == expected)
}

/// Read a file's contents, treating "does not exist" as an empty string.
fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Byte range `(start_of_begin_marker, end_of_end_marker)` if both markers
/// are present and in order; `None` otherwise (including the malformed case
/// where only one marker exists).
fn find_block_bounds(content: &str) -> Option<(usize, usize)> {
    let begin = content.find(BEGIN_MARKER)?;
    let after_begin = begin + BEGIN_MARKER.len();
    let end_rel = content[after_begin..].find(END_MARKER)?;
    let mut end = after_begin + end_rel + END_MARKER.len();
    // Consume one trailing newline after the end marker too, since `block`
    // already supplies its own — otherwise re-applying doubles it.
    if content[end..].starts_with('\n') {
        end += 1;
    }
    Some((begin, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_creates_agents_md_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = apply(tmp.path()).unwrap();

        assert_eq!(path, tmp.path().join("AGENTS.md"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with(BEGIN_MARKER));
        assert!(content.trim_end().ends_with(END_MARKER));
        assert!(content.contains(&managed_block()));
    }

    #[test]
    fn apply_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();

        apply(tmp.path()).unwrap();
        let first = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();

        apply(tmp.path()).unwrap();
        let second = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn apply_preserves_unrelated_content_outside_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        let original = "# My Project\n\
             \n\
             Some intro paragraph explaining what this repo does.\n\
             \n\
             ## Conventions\n\
             \n\
             More project-specific notes go here.\n";
        std::fs::write(&path, original).unwrap();

        apply(tmp.path()).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(original));
        assert!(after.contains(BEGIN_MARKER));
        assert!(after.contains(END_MARKER));
        assert!(after.contains(&managed_block()));

        // Applying again must not disturb the unrelated content or grow the file.
        apply(tmp.path()).unwrap();
        let after_twice = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, after_twice);
        assert!(after_twice.starts_with(original));
    }

    #[test]
    fn apply_replaces_only_interior_when_markers_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        let original = format!(
            "# Before\n\n{BEGIN_MARKER}\nstale content that should be replaced\n{END_MARKER}\n\n# After\n"
        );
        std::fs::write(&path, &original).unwrap();

        apply(tmp.path()).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with("# Before\n\n"));
        assert!(after.ends_with("\n\n# After\n"));
        assert!(!after.contains("stale content"));
        assert!(after.contains(&managed_block()));
    }

    #[test]
    fn ensure_gitignore_creates_file_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_gitignore(tmp.path()).unwrap();

        let content = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(content.lines().any(|l| l.trim() == ".pact/leases/"));
    }

    #[test]
    fn is_current_reflects_block_freshness() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_current(tmp.path()).unwrap()); // no AGENTS.md yet

        apply(tmp.path()).unwrap();
        assert!(is_current(tmp.path()).unwrap());

        let path = tmp.path().join("AGENTS.md");
        let stale = std::fs::read_to_string(&path)
            .unwrap()
            .replace("coordination protocol", "COORDINATION PROTOCOL (edited)");
        std::fs::write(&path, stale).unwrap();
        assert!(!is_current(tmp.path()).unwrap());
    }

    #[test]
    fn ensure_gitignore_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();

        ensure_gitignore(tmp.path()).unwrap();
        ensure_gitignore(tmp.path()).unwrap();

        let content = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        let count = content
            .lines()
            .filter(|l| l.trim().trim_end_matches('/') == ".pact/leases")
            .count();
        assert_eq!(count, 1);
        assert!(content.contains("target/"));
    }
}
