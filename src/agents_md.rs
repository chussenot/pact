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
  variable (or `--agent <name>`). Set one before running pact commands; pact
  never guesses an identity. `pact whoami` shows the identity and paths it
  resolved.
- **Announce intent before you research, not just before you write.** Your
  first pact commands come *before* you read the first file: `pact msg inbox`
  and `pact lease ls` to see what is already claimed and by whom, then
  `pact msg send --to <peer-or-human>` saying what you are about to work on,
  then `pact lease acquire <path>... --note "<what>"` for the files you expect
  to own. Several paths in one `acquire` are taken all-or-nothing, so you never
  end up holding half of what you need while a peer holds the rest.
  Do it even if you will only be reading for the next ten minutes. Why:
  a peer planning against the same file can renegotiate now instead of at the
  end, when both plans are sunk cost — and a fleet that has announced nothing
  looks exactly like a fleet that crashed on startup.
- **Ownership, and its one carve-out, stated together**: lease every file you
  edit that another agent might also touch, and release it when done. The
  single exception is a file that is yours alone by assignment (your own
  evidence log, your own scratch dir) — nobody else writes it, so it needs no
  lease. Anything else: lease it. Leases are advisory, not enforced by the
  filesystem; respect them anyway.
- **Keep a lease alive, then let it all go**: a lease lasts `--ttl` seconds
  (default 900) and `pact lease renew <path>` refreshes it — a long task must
  not outlive its lease. `pact lease release <path>` frees one file, `pact
  lease release --all` frees everything you hold in a single call, so nothing
  gets half-forgotten. Release before you report yourself finished, not after.
- **A path someone else holds exits 2** — branch on that, not on the message
  text. `pact lease ls` names the holder; message them and pick up something
  else, which is what announcing early bought you. `pact lease acquire --steal`
  and `pact lease release --force` do override a live claim, but both warn on
  stderr and name the agent they displaced: reach for them when you know a peer
  is gone, not when you are impatient with one who isn't.
- **Announce contract changes**: if you change an API, schema, CLI flag, or
  any other contract another agent depends on, message them:
  `pact msg send --to <agent> "what changed and why"`. Check the recipient
  exists with `pact agents` first — a mistyped name sends into the void. One
  decision that affects several agents goes out as ONE message: repeat `--to`
  and they all land in a single thread anyone can read and reply into.
- **Use a file for anything longer than a sentence**: `--body-file <path>`, or
  `--body-file -` for stdin. Quotes, backslashes and aligned tables do not
  survive a shell, and handing over an API is exactly that kind of content.
- **Read and reply in the same thread**: `pact msg inbox` lists one line per
  message; `pact msg read <id>` shows one in full together with its whole
  thread. Reply with `pact msg send --to <sender> --thread <id> "..."` — a
  reply sent without `--thread` starts a new thread, and the exchange stops
  being readable as one conversation.
- **Confirm, don't re-send**: `pact msg sent` shows what you sent and whether
  the recipient has read it. If you are unsure a message went out, check
  there — a blind re-send is how a peer's inbox fills with duplicates.
- **Orient with `pact log`**: one chronological feed of who leased what and
  who said what. Read it when you join, and when you need to know whether a
  peer is still moving.
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
    splice_block(&path, &managed_block())?;
    Ok(path)
}

/// Idempotently splice `body` between the pact markers in `path`, creating the
/// file if absent and leaving every byte outside the markers alone.
fn splice_block(path: &Path, body: &str) -> Result<()> {
    let existing = read_or_empty(path)?;

    let block = format!("{BEGIN_MARKER}\n{body}{END_MARKER}\n");

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

    std::fs::write(path, new_content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The line that pulls `AGENTS.md` into `CLAUDE.md`. Claude Code resolves a
/// bare `@<path>` in a memory file by inlining that file's contents.
pub const CLAUDE_IMPORT: &str = "@AGENTS.md";

/// What [`ensure_claude_md`] found or did, so `init` can report it honestly
/// instead of claiming to have written a file it deliberately skipped.
pub enum ClaudeMd {
    /// The pact-managed import block is now present in `CLAUDE.md`.
    Managed(PathBuf),
    /// `CLAUDE.md` already pulls in `AGENTS.md` by a line we did not write, so
    /// adding ours would inline the same file twice.
    AlreadyImported(PathBuf),
    /// `CLAUDE.md` resolves to `AGENTS.md` itself (the symlink layout). The
    /// protocol is already in what Claude Code loads, and writing
    /// `@AGENTS.md` into `AGENTS.md` would be a self-import.
    SameFileAsAgentsMd,
}

/// Make the protocol reachable by Claude Code, which loads `CLAUDE.md`,
/// `.claude/CLAUDE.md`, `CLAUDE.local.md` and `.claude/rules/` — but **not**
/// `AGENTS.md`. Without this, `pact init` in a fresh repo produced an
/// `AGENTS.md` that Claude never read, so a Claude-driven fleet silently
/// skipped the whole protocol: no leases, no inbox, no announcements.
///
/// Imports rather than copies, so there is still exactly one source of truth.
pub fn ensure_claude_md(repo_root: &Path) -> Result<ClaudeMd> {
    let path = repo_root.join("CLAUDE.md");
    let agents = repo_root.join("AGENTS.md");

    // canonicalize() resolves symlinks; equal targets mean one file under two
    // names. (Hard links are not detected — a symlink is the layout people
    // actually use, and bd's own guidance suggests it.)
    if let (Ok(a), Ok(b)) = (path.canonicalize(), agents.canonicalize()) {
        if a == b {
            return Ok(ClaudeMd::SameFileAsAgentsMd);
        }
    }

    let existing = read_or_empty(&path)?;
    let has_import = existing.lines().any(|l| l.trim() == CLAUDE_IMPORT);
    // An import inside our own block is ours to keep managing; one outside it
    // is the user's, and adding a second would inline AGENTS.md twice.
    let import_is_ours = match find_block_bounds(&existing) {
        Some((begin, end)) => existing[begin..end].contains(CLAUDE_IMPORT),
        None => false,
    };
    if has_import && !import_is_ours {
        return Ok(ClaudeMd::AlreadyImported(path));
    }

    splice_block(&path, &claude_block())?;
    Ok(ClaudeMd::Managed(path))
}

/// The block written into `CLAUDE.md`: a pointer, never a second copy of the
/// protocol. Two copies would drift, and `is_current` could only police one.
fn claude_block() -> String {
    format!(
        "## pact coordination protocol\n\
         \n\
         Claude Code loads this file, not `AGENTS.md`, so the protocol is imported\n\
         here instead of copied — one source of truth, in the file the other agents\n\
         already read. Run `pact init` to refresh it.\n\
         \n\
         {CLAUDE_IMPORT}\n"
    )
}

/// Whether Claude Code can actually reach the protocol: either `CLAUDE.md`
/// imports `AGENTS.md`, or the two are the same file (used by `pact doctor`).
pub fn claude_md_reaches_protocol(repo_root: &Path) -> Result<bool> {
    let path = repo_root.join("CLAUDE.md");
    let agents = repo_root.join("AGENTS.md");
    if let (Ok(a), Ok(b)) = (path.canonicalize(), agents.canonicalize()) {
        if a == b {
            return Ok(true);
        }
    }
    Ok(read_or_empty(&path)?
        .lines()
        .any(|l| l.trim() == CLAUDE_IMPORT))
}

/// Add a single `.pact/` line to `.gitignore`, idempotently, only if missing.
/// Everything under `.pact/` is local runtime state — leases, message read
/// state, and whatever else pact or an agent writes there — so the rule is the
/// directory, not an enumeration of filenames. A repo that already ignores
/// `.pact/`, or that carries the legacy pair (`.pact/leases/` plus
/// `.pact/read.json`), is already covered and is left untouched.
pub fn ensure_gitignore(repo_root: &Path) -> Result<()> {
    let path = repo_root.join(".gitignore");
    let existing = read_or_empty(&path)?;

    let (mut dir, mut leases, mut read_state) = (false, false, false);
    for line in existing.lines() {
        match line.trim().trim_end_matches('/') {
            ".pact" => dir = true,
            ".pact/leases" => leases = true,
            ".pact/read.json" => read_state = true,
            _ => {}
        }
    }
    if dir || (leases && read_state) {
        return Ok(());
    }

    let mut new_content = existing;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(".pact/\n");

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

    /// A hand-written `@AGENTS.md` must not get a second, pact-managed one:
    /// two imports inline the whole file twice into Claude's context.
    #[test]
    fn ensure_claude_md_leaves_a_hand_written_import_alone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Mine\n\n@AGENTS.md\n").unwrap();

        let got = ensure_claude_md(tmp.path()).unwrap();
        assert!(matches!(got, ClaudeMd::AlreadyImported(_)));

        let after = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert_eq!(after, "# Mine\n\n@AGENTS.md\n", "file must be untouched");
        assert!(claude_md_reaches_protocol(tmp.path()).unwrap());
    }

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
        assert!(content.lines().any(|l| l.trim() == ".pact/"));
    }

    /// The whole point of pact-rnc.16: agent-written files under `.pact/`
    /// (e.g. `.pact/evidence/*`) must not need a new gitignore rule each.
    #[test]
    fn ensure_gitignore_does_not_duplicate_existing_rules() {
        let cases = [
            ".pact/\n",
            ".pact\n",
            "target/\n.pact/leases/\n.pact/read.json\n", // legacy pair
        ];
        for original in cases {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(".gitignore");
            std::fs::write(&path, original).unwrap();

            ensure_gitignore(tmp.path()).unwrap();

            let after = std::fs::read_to_string(&path).unwrap();
            assert_eq!(after, original, "should have been left alone: {original:?}");
        }
    }

    /// An agent only knows the commands this block tells it about.
    #[test]
    fn managed_block_teaches_the_protocol_commands() {
        let block = managed_block();
        // Every command an agent needs to *operate* under the protocol. The
        // block drifted behind the CLI twice (pact-sri, then multi-path
        // acquire, which that bead listed and its fix missed), so the list is
        // asserted rather than reviewed by eye.
        for needle in [
            "before you research",
            "pact msg inbox",
            "pact msg read",
            "--thread",
            "pact msg sent",
            "pact lease ls",
            "pact lease acquire",
            "acquire <path>...",
            "pact lease renew",
            "--ttl",
            "release --all",
            "--steal",
            "--force",
            "exits 2",
            "pact log",
            "pact agents",
            "pact whoami",
            "--body-file",
            "--json",
        ] {
            assert!(block.contains(needle), "managed_block missing {needle:?}");
        }
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
            .filter(|l| l.trim().trim_end_matches('/') == ".pact")
            .count();
        assert_eq!(count, 1);
        assert!(content.contains("target/"));
    }
}
