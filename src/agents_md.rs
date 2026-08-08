//! Idempotent managed section in `AGENTS.md` teaching agents the pact
//! coordination protocol. Never touches content outside the markers.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

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
- **Also export `BEADS_ACTOR=$PACT_AGENT`, once, in the same shell.** pact's
  own `--actor=<agent>` attribution only covers bd calls pact itself makes —
  it never reaches `bd ready`/`bd update --claim`/`bd close`, which you run
  directly. Without this, every one of those commands falls through to bd's
  own next attribution tier — your shared checkout's `git user.name` — so a
  15-agent fleet's entire task-tracking history can attribute to one identity
  while `.pact/events.jsonl` correctly shows sixteen. `pact whoami` prints the
  exact line to run.
- **Announce intent before you research, not just before you write.** Your
  first pact commands come *before* you read the first file: `pact msg inbox`
  and `pact lease ls` to see what is already claimed and by whom, then
  `pact lease acquire <path>... --note "<what you are doing and why>"` for the
  files you expect to own. Several paths in one `acquire` are taken
  all-or-nothing, so you never end up holding half of what you need while a
  peer holds the rest. Do it even if you will only be reading for the next ten
  minutes. Why: a peer planning against the same file can renegotiate now
  instead of at the end, when both plans are sunk cost — and a fleet that has
  announced nothing looks exactly like a fleet that crashed on startup.
- **The lease note IS the announcement — do not also message it.** `pact log`
  already records every acquire, renew, release and expiry with its note, and
  `pact ui` shows that live, so a human watching already sees what you claimed
  and why. A message saying "starting on src/foo.rs" duplicates a record that
  wrote itself.
  **Send a message when you need something back**: a decision, a file you do
  not own, a warning about a contract you changed. Not to report progress.
  Measured on one fleet: 85 messages, 41 of them status pings to `human`, and
  an inbox nobody could triage — which is how a real `BLOCKER` message sat
  unread for 38 minutes in the middle of it.
- **Lease anything you WRITE, not just files you edit.** A lease is on a path,
  so a directory of shared state is leasable too — `pact lease acquire .beads/
  --note "probing the br CLI"` before you run a tool that might write there.
  An agent that had correctly leased both source files it edited still
  corrupted the shared Beads store, because it read the protocol as being about
  editing files and a CLI wrote a second database behind it at exit 0.
- **Ownership, and its one carve-out, stated together**: lease every file you
  edit that another agent might also touch, and release it when done. The
  single exception is a file that is yours alone by assignment (your own
  evidence log, your own scratch dir) — nobody else writes it, so it needs no
  lease. Anything else: lease it. Leases are advisory, not enforced by the
  filesystem; respect them anyway.
- **Let it all go when you are done**: the default lease is 45 minutes; for
  genuinely longer work, acquire with `--ttl` or `pact lease renew <path>`. That
  default is measured, not guessed — `pact audit` put the p90 hold at 24 minutes
  and the longest ever at 36, against one renewal in the entire history. So most
  work never needs to think about the TTL at all. `pact lease release <path>`
  frees one file, `pact lease release --all` frees everything you hold in a
  single call, so nothing gets half-forgotten. Release before you report
  yourself finished, not after.
- **Ask whose file it is before you touch it, and hand it back by name**:
  `pact agents --for <path>` names the last agent to act on a path even after
  they released it and exited, and `pact lease acquire` tells you the same
  thing unprompted. When you need something from that agent, address the FILE,
  not the name: `pact msg send --to-owner-of <path> "..."`. A path outlives the
  process that held it, so a handoff sent to a path still reaches whoever picks
  it up next; one sent to an agent that has finished is a dead letter.
- **A message about a file follows the file.** `pact msg send --to-owner-of
  <path>` does not just look up a name — the message is tagged with the path,
  and whoever leases that path next is told it is waiting, even if the agent it
  resolved to has exited. So when you are handing off work, address the FILE.
  And read what `pact lease acquire` tells you before you edit: a message
  waiting on a path is usually the reason the last agent stopped.
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
- **Commit `.pact/events.jsonl` when you commit your work.** It is the
  append-only record behind `pact log`, it is the one thing under `.pact/` that
  is NOT gitignored, and it is the only thing pact stores that it cannot derive
  from anything else. `.pact/leases/` and `.pact/waits/` stay local — those are
  live runtime state and committing them would have you fighting over peers'
  in-flight claims. Fold the log into the commit whose work produced the events;
  a missed one is self-healing on the next commit. Left uncommitted, every clone
  of this repo starts with no coordination history at all, and nobody can ask
  afterwards who held what or whether two agents ever held one path at once.
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
    splice_block(&path, &managed_block(), repo_root)?;
    Ok(path)
}

/// Idempotently splice `body` between the pact markers in `path`, creating the
/// file if absent and leaving every byte outside the markers alone.
///
/// Goes through [`write_atomic_cas`] rather than a plain read-then-write: see
/// that function for why a bare read/compute/rename can silently discard a
/// concurrent edit.
fn splice_block(path: &Path, body: &str, repo_root: &Path) -> Result<()> {
    let block = format!("{BEGIN_MARKER}\n{body}{END_MARKER}\n");

    write_atomic_cas(path, repo_root, |existing| {
        match find_block_bounds(existing) {
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
                let mut s = existing.to_string();
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
        }
    })
}

/// The line that pulls `AGENTS.md` into `CLAUDE.md`. Claude Code resolves a
/// bare `@<path>` in a memory file by inlining that file's contents. Gemini
/// CLI's memory import processor and GitHub Copilot CLI spell it the same way,
/// so [`INSTRUCTION_TARGETS`] reuses this line rather than inventing a second.
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

    splice_block(&path, &claude_block(), repo_root)?;
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

/// Agent-instruction filenames other than `AGENTS.md` and `CLAUDE.md` that
/// pact points back at `AGENTS.md`, paired with whether the format expands a
/// bare `@<path>` import line. The flag is researched per tool rather than
/// assumed, because guessing wrong costs something either way: a dangling
/// `@AGENTS.md` in a format that ignores it reads like a broken link, and the
/// alternative people reach for — inlining the protocol — is the copy that
/// [`is_current`] exists to catch and can only police in one file.
///
/// - `GEMINI.md` — Gemini CLI's memory import processor inlines `@file.md`.
/// - `.github/copilot-instructions.md` — Copilot CLI expands `@<relative
///   path>`; VS Code's Copilot does not (microsoft/vscode#246877). It gets the
///   import line *and* the prose directive, so both halves of Copilot are
///   covered by one block.
/// - `.cursorrules`, `.windsurfrules`, `.clinerules` — flat files with no
///   import mechanism, so prose is all there is. That is still a reference and
///   not a copy: these are agents with file-read tools, and "go read
///   AGENTS.md" is an instruction they can execute.
///
/// Deliberately absent: `.cursor/rules/`. Managing it means *creating* a new
/// `.mdc` rule file, which the "only manage what already exists" rule forbids,
/// and an `.mdc` without the right frontmatter (`alwaysApply`, or a
/// `description` for Cursor to match on) is silently never applied — a rule
/// pact writes and Cursor ignores is worse than no rule, because it looks done.
const INSTRUCTION_TARGETS: &[(&str, bool)] = &[
    ("GEMINI.md", true),
    (".github/copilot-instructions.md", true),
    (".cursorrules", false),
    (".windsurfrules", false),
    (".clinerules", false),
];

/// The block spliced into a non-Claude instruction file: a reference, never a
/// second copy of the protocol.
fn pointer_block(expands_imports: bool) -> String {
    let mut s = String::from(
        "## pact coordination protocol\n\
         \n\
         Read `AGENTS.md` in the root of this repository and follow its \"pact\n\
         coordination protocol\" section before you touch shared files or hand\n\
         off work. It is referenced here, not copied, so this file has nothing\n\
         to drift out of date with. Run `pact init` to refresh it.\n",
    );
    if expands_imports {
        s.push_str(&format!("\n{CLAUDE_IMPORT}\n"));
    }
    s
}

/// The known instruction targets that this repo actually has. Existence *is*
/// the configuration: pact v1 ships no config file, and creating
/// `.windsurfrules` in a repo that has never seen Windsurf would be pact
/// inventing a tool the team does not use. (`CLAUDE.md` is the one file pact
/// creates when absent — see [`ensure_claude_md`] for why.)
///
/// `is_file`, not `exists`, because `.clinerules` is also allowed to be a
/// *directory*; splicing a block into a directory path just errors.
///
/// A target that *is* `AGENTS.md` under another name is skipped for the same
/// reason [`ensure_claude_md`] skips it — and this filter is why the check
/// lives here rather than in one caller. `is_file()` follows symlinks, so
/// `GEMINI.md -> AGENTS.md` looked like an ordinary target and
/// [`ensure_instruction_files`] spliced the *pointer* block through the link,
/// destroying the protocol block `apply` had written seconds earlier in the
/// same `pact init`. It never converged: the pointer always wrote last, so
/// `pact doctor` reported the block stale and prescribed the command doing the
/// damage. Symlinking every tool's file at `AGENTS.md` is the whole agents-md
/// convention, so this is a normal layout, not an exotic one.
///
/// The same collision can happen between two *targets* rather than a target
/// and `AGENTS.md` — `GEMINI.md` and `.cursorrules` symlinked to each other,
/// or both to some third shared file. Both look like ordinary candidates, so
/// splicing each independently means the second write clobbers the first's
/// block, and `pact doctor` reports the first stale immediately after `init`
/// claimed to update it. So: once a candidate's canonical path has already
/// been claimed by an earlier entry in `INSTRUCTION_TARGETS`, skip it rather
/// than write it — the same "skip the alias" choice already made for
/// AGENTS.md, just applied across the whole list instead of one fixed target.
fn present_targets(repo_root: &Path) -> Vec<(PathBuf, bool)> {
    let agents = repo_root.join("AGENTS.md").canonicalize().ok();
    let mut seen = std::collections::HashSet::new();
    INSTRUCTION_TARGETS
        .iter()
        .map(|(name, expands_imports)| (repo_root.join(name), *expands_imports))
        .filter(|(path, _)| path.is_file())
        .filter(|(path, _)| agents.is_none() || path.canonicalize().ok() != agents)
        .filter(|(path, _)| match path.canonicalize().ok() {
            Some(canon) => seen.insert(canon),
            None => true,
        })
        .collect()
}

/// Every instruction file pact manages in this repo, current or not, so `pact
/// doctor` can say "none present" instead of showing a green tick for nothing.
pub fn managed_instruction_files(repo_root: &Path) -> Vec<PathBuf> {
    present_targets(repo_root)
        .into_iter()
        .map(|(p, _)| p)
        .collect()
}

/// Point every already-present agent-instruction file at `AGENTS.md`, and
/// return the ones touched. Idempotent, and never writes outside the markers.
///
/// Without this, an agent whose tool reads `GEMINI.md` or
/// `.github/copilot-instructions.md` joined a pact fleet having never been
/// told the protocol exists — the same failure `ensure_claude_md` fixed for
/// Claude Code, one file at a time.
pub fn ensure_instruction_files(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut managed = Vec::new();
    for (path, expands_imports) in present_targets(repo_root) {
        splice_block(&path, &pointer_block(expands_imports), repo_root)?;
        managed.push(path);
    }
    Ok(managed)
}

/// Managed instruction files whose pact block is missing or stale, so `pact
/// doctor` has the same opinion about `GEMINI.md` it already has about
/// `CLAUDE.md`. A file pact writes but never re-checks goes stale in silence.
pub fn stale_instruction_files(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut stale = Vec::new();
    for (path, expands_imports) in present_targets(repo_root) {
        if !has_current_block(&path, &pointer_block(expands_imports))? {
            stale.push(path);
        }
    }
    Ok(stale)
}

/// Ignore the *runtime* parts of `.pact/`, and deliberately not the event log.
///
/// This used to ignore `.pact/` wholesale, on the reasoning that everything under
/// it is local runtime state. That was right about leases and wrong about
/// history. A lease is a claim on a path *right now* — meaningless in a clone,
/// and committing lock files would have agents fighting over each other's
/// in-flight claims. [`EVENTS_LOG_PATH`] is the opposite: an append-only record
/// of what happened, and the only thing pact stores that it cannot derive.
/// Ignoring it meant every clone began with **zero** coordination history, so no
/// question about how a fleet behaved could be asked after the fact. That is the
/// same mistake gitignoring `.beads/interactions.jsonl` would be, for the same
/// reason — applied this time to pact's own data.
///
/// **A re-run migrates an older repo.** A repo initialised before this change has
/// `.pact/` in its `.gitignore`, and leaving that alone would keep the history
/// lost for precisely the repos that have accumulated the most of it. `pact init`
/// already owns the lines it wrote, so the broad entry is replaced in place with
/// the specific runtime paths and everything else in the file is left untouched.
///
/// Idempotent both ways: a second run finds the narrow rules present and writes
/// nothing.
pub fn ensure_gitignore(repo_root: &Path) -> Result<()> {
    let path = repo_root.join(".gitignore");
    // Through write_atomic_cas, not a plain read-then-write (pact-m7j.9.11):
    // gitignore_content recomputes its answer from whatever is actually on
    // disk at commit time, so a concurrent hand-edit between the read and
    // the rename is re-read and folded in on retry instead of silently
    // discarded — the same race splice_block was fixed against in
    // pact-m7j.9.2.
    write_atomic_cas(&path, repo_root, gitignore_content)
}

/// Pure computation half of [`ensure_gitignore`], split out so a test can
/// wrap it in [`write_atomic_cas`] directly and inject a race the same way
/// the generic CAS tests do, instead of only exercising `ensure_gitignore`
/// end to end with no injection point of its own.
fn gitignore_content(existing: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let (mut narrowed, mut already) = (false, false);

    for line in existing.lines() {
        let bare = line.trim().trim_end_matches('/');
        if bare == ".pact" {
            // The broad rule from an older pact. Replaced rather than
            // appended to, so the file does not end up ignoring both the
            // directory and a subset of it — which reads as though somebody
            // was unsure.
            out.push(RUNTIME_IGNORE_COMMENT_1.to_string());
            out.push(RUNTIME_IGNORE_COMMENT_2.to_string());
            out.extend(RUNTIME_IGNORE_RULES.iter().map(|r| (*r).to_string()));
            narrowed = true;
            continue;
        }
        if line.trim() == RUNTIME_IGNORE_SENTINEL {
            already = true;
        }
        out.push(line.to_string());
    }

    if narrowed {
        // Fall through and write.
    } else if already {
        // Byte-for-byte what was already there — write_atomic_cas still
        // renames it, same as splice_block does on an already-current
        // AGENTS.md, rather than adding a second "is a write actually
        // needed" branch here to skip it.
        return existing.to_string();
    } else {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(RUNTIME_IGNORE_COMMENT_1.to_string());
        out.push(RUNTIME_IGNORE_COMMENT_2.to_string());
        out.extend(RUNTIME_IGNORE_RULES.iter().map(|r| (*r).to_string()));
    }

    let mut content = out.join("\n");
    content.push('\n');
    content
}

/// Ignore everything under `.pact/`, then re-include exactly the event log.
///
/// **Deny by default, and that ordering is the whole design.** The first draft of
/// this change listed the runtime paths instead — `.pact/leases/`,
/// `.pact/waits/` — which reads as more precise and is much worse. It silently
/// dropped the property pact-rnc.16 exists for: a file an agent invents under
/// `.pact/` must be ignored without anyone adding a rule for it. Running that
/// draft on pact's own repository staged 31 evidence logs and a file containing a
/// live `SIGNOZ_API_KEY`, because those had been covered by the broad rule and
/// suddenly were not. An allow-list of what to hide is a list somebody has to
/// keep complete; a deny-list with one exception is not.
///
/// The negation works because `.pact/*` ignores the *contents* of `.pact/` rather
/// than the directory itself — git still descends into it, so re-including a file
/// directly inside is allowed. (A rule of `.pact/` would ignore the directory
/// outright and no `!` line beneath could reach in.) Verified with
/// `git check-ignore` rather than reasoned about, in tests/events_log.rs.
const RUNTIME_IGNORE_RULES: &[&str] = &[".pact/*", "!.pact/events.jsonl"];

/// The line that decides whether the rules are already present. Keyed on the
/// deny line: a repo with `.pact/*` has been through this.
const RUNTIME_IGNORE_SENTINEL: &str = ".pact/*";

const RUNTIME_IGNORE_COMMENT_1: &str =
    "# Everything pact or an agent writes under .pact/ is local runtime state,";
const RUNTIME_IGNORE_COMMENT_2: &str =
    "# EXCEPT the append-only event log, which is history and belongs in git.";

/// `.pact/events.jsonl`: the one file under `.pact/` that belongs in git.
pub const EVENTS_LOG_PATH: &str = ".pact/events.jsonl";

/// `merge=union` for the event log, so that committing an append-only file does
/// not mean a conflict on every merge.
///
/// Included because without it, narrowing the ignore rule recreates the very
/// problem that motivated ignoring `.pact/` in the first place: two agents on two
/// branches append, git sees both sides changed the same trailing region, and
/// every merge stops for a human who has nothing to decide. `merge=union` keeps
/// both sides, which is the right resolution for a log whose entries are
/// independent and whose inter-agent ordering carries no meaning. Verified: two
/// branches appending different events merge with no conflict.
///
/// An existing rule for this path is left alone — that one really is the user's
/// call, since a different merge driver is a deliberate choice.
pub fn ensure_gitattributes(repo_root: &Path) -> Result<()> {
    let path = repo_root.join(".gitattributes");
    // Through write_atomic_cas, same reason as ensure_gitignore just above
    // (pact-m7j.9.11).
    write_atomic_cas(&path, repo_root, gitattributes_content)
}

/// Pure computation half of [`ensure_gitattributes`] — see
/// [`gitignore_content`]'s doc comment for why this is split out.
fn gitattributes_content(existing: &str) -> String {
    if existing
        .lines()
        .any(|l| l.split_whitespace().next() == Some(EVENTS_LOG_PATH))
    {
        return existing.to_string();
    }

    let mut out: Vec<String> = existing.lines().map(str::to_string).collect();
    if !out.is_empty() {
        out.push(String::new());
    }
    out.push("# pact: the event log is append-only, so a merge keeps BOTH sides".to_string());
    out.push("# rather than stopping for a human who has nothing to decide.".to_string());
    out.push(format!("{EVENTS_LOG_PATH} merge=union"));

    let mut content = out.join("\n");
    content.push('\n');
    content
}

/// Whether AGENTS.md exists, has a managed block, and that block matches the
/// current `managed_block()` exactly (used by `pact doctor`).
pub fn is_current(repo_root: &Path) -> Result<bool> {
    has_current_block(&repo_root.join("AGENTS.md"), &managed_block())
}

/// Whether `path` already carries exactly the block `splice_block` would write
/// for `body` — i.e. whether `pact init` would be a no-op for that file.
fn has_current_block(path: &Path, body: &str) -> Result<bool> {
    let existing = read_or_empty(path)?;
    let Some((begin, end)) = find_block_bounds(&existing) else {
        return Ok(false);
    };
    Ok(existing[begin..end] == format!("{BEGIN_MARKER}\n{body}{END_MARKER}\n"))
}

/// Follow a symlink to its target before replacing anything. `fs::write`
/// writes THROUGH a link; `rename` would replace the link itself with a
/// regular file, silently disconnecting a `CLAUDE.md` that somebody had
/// pointed at their dotfiles. Atomicity is the change [`write_atomic_cas`]
/// makes; which file gets written is not, so resolve first and keep the old
/// meaning. (A link that IS `AGENTS.md` under another name never reaches
/// here — `present_targets` skips those aliases.)
///
/// A resolved target outside `repo_root` gets a warning, every time, rather
/// than either silently writing through it or refusing. Refusing would break
/// the legitimate case this exists to support — `CLAUDE.md` symlinked to
/// e.g. `~/dotfiles/CLAUDE.md`, deliberately outside the repo — and nothing
/// distinguishes that from an accidental symlink (a bad merge, a restored
/// backup, a copy-pasted template, a stray `ln -s`) by location alone: both
/// share the identical shape. Reproduced live: symlinking `AGENTS.md` to
/// `../victim-outside-repo.md` and running `pact init --no-commit` spliced
/// pact's protocol block into the victim file, exit 0, nothing in the output
/// naming what happened. This names both the nominal path and the resolved
/// target, so that output is the thing that catches it instead.
fn resolve_write_target(path: &Path, repo_root: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let is_symlink = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink {
        let root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        if !resolved.starts_with(&root) {
            crate::output::warn(&format!(
                "warning: {} is a symlink to {}, which is outside the repository at {} — \
                 writing through it anyway; if this is not the intentional dotfiles-style \
                 layout, check where it points",
                path.display(),
                resolved.display(),
                root.display()
            ));
        }
    }
    resolved
}

/// The read-only half of the check [`resolve_write_target`] performs before
/// writing: is `path` a symlink whose target resolves outside `repo_root`,
/// and if so, where does it point?
///
/// Split out so `pact doctor` can ask the same question `pact init` warns
/// about at write time, without running `init` and without printing anything
/// itself (pact-m7j.9.12) — `init`'s warning only ever fires mid-write, so a
/// repo nobody has re-run `init` in stayed silent about an escaping symlink
/// until the next write happened to touch it.
fn escaping_symlink(path: &Path, repo_root: &Path) -> Option<PathBuf> {
    let is_symlink = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if !is_symlink {
        return None;
    }
    let resolved = std::fs::canonicalize(path).ok()?;
    let root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    (!resolved.starts_with(&root)).then_some(resolved)
}

/// Every managed write-set file that is a symlink resolving outside
/// `repo_root` — every instruction file [`managed_instruction_files`] tracks,
/// plus `AGENTS.md`, `CLAUDE.md`, `.gitignore` and `.gitattributes`, the four
/// files pact writes directly rather than through [`present_targets`]. Paired
/// with the resolved target, so `pact doctor` can name both halves the same
/// way `init`'s own warning does (pact-m7j.9.12).
pub fn escaping_write_set_symlinks(repo_root: &Path) -> Vec<(PathBuf, PathBuf)> {
    managed_instruction_files(repo_root)
        .into_iter()
        .chain(
            ["AGENTS.md", "CLAUDE.md", ".gitignore", ".gitattributes"]
                .iter()
                .map(|f| repo_root.join(f)),
        )
        .filter_map(|p| escaping_symlink(&p, repo_root).map(|target| (p, target)))
        .collect()
}

/// Bounded attempts for [`write_atomic_cas`]'s retry loop, after which it
/// fails loudly rather than spinning forever against a file under constant
/// writes.
const MAX_CAS_ATTEMPTS: u32 = 5;

/// An atomic write for a read-modify-write: `modify` computes the new content
/// from what is actually on disk, and may be called again with fresher
/// content if a concurrent writer lands between the read `modify` was given
/// and the commit-moment rename.
///
/// The plain read-then-write this replaced had no lock and no version check:
/// pausing between its read and its rename (reproduced live with strace delay
/// injection) let a concurrent write to `AGENTS.md` complete during the
/// pause, and the delayed rename then silently and completely overwrote it —
/// no error, no warning, no event logged. This closes that window: content is
/// re-read immediately before the rename, and if it no longer matches what
/// `modify` was given, that attempt's output is discarded and `modify` is
/// re-run against the fresh content instead of clobbering it. After
/// [`MAX_CAS_ATTEMPTS`] straight conflicts it gives up with a loud error
/// rather than loop forever against a file under constant writes.
fn write_atomic_cas(
    path: &Path,
    repo_root: &Path,
    mut modify: impl FnMut(&str) -> String,
) -> Result<()> {
    // Resolved once: the symlink target itself is not expected to move mid
    // retry-loop, and re-resolving per attempt would repeat the escaped-target
    // warning once per conflict instead of once per call.
    let resolved = resolve_write_target(path, repo_root);
    let dir = resolved.parent().unwrap_or(Path::new("."));

    let mut before = read_or_empty(path)?;
    for attempt in 1..=MAX_CAS_ATTEMPTS {
        let new_content = modify(&before);

        let tmp = dir.join(crate::events::unique_temp_name(".pact-write"));
        std::fs::write(&tmp, &new_content).with_context(|| format!("writing {}", tmp.display()))?;
        if let Ok(meta) = std::fs::metadata(&resolved) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }

        // The commit-moment check: has the file changed since `before` was
        // read? If so, `new_content` was computed from data that is no longer
        // current — discard it and retry from what is actually on disk now,
        // rather than rename over a newer write.
        let now = read_or_empty(path)?;
        if now != before {
            let _ = std::fs::remove_file(&tmp);
            if attempt == MAX_CAS_ATTEMPTS {
                bail!(
                    "{} changed concurrently while pact was writing it ({attempt} attempts in a \
                     row); giving up rather than risk overwriting the latest edit",
                    path.display()
                );
            }
            before = now;
            continue;
        }

        return match std::fs::rename(&tmp, &resolved) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e).with_context(|| format!("replacing {}", resolved.display()))
            }
        };
    }
    unreachable!("loop above always returns by the last attempt")
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

    /// Deterministic proof of the compare-and-swap fix — no thread, no sleep,
    /// no timing luck. The `modify` closure itself performs the "concurrent
    /// write" partway through its own call, which is exactly the interleaving
    /// the incident reproduced by pausing pact (via strace delay injection)
    /// between its read and its commit-moment rename: a write lands in that
    /// gap. Without the CAS recheck, `modify` is called once, and whatever it
    /// returns — computed from the PRE-race content — is written and renamed
    /// over the concurrent edit unconditionally, discarding it. With the
    /// fix, the recheck right before the rename catches the mismatch, and
    /// `modify` is called again against the fresh content instead.
    #[test]
    fn write_atomic_cas_never_commits_over_a_write_that_landed_mid_call() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        std::fs::write(&path, "# Notes\n\noriginal\n").unwrap();

        let mut calls = 0u32;
        write_atomic_cas(&path, tmp.path(), |existing| {
            calls += 1;
            if calls == 1 {
                // The concurrent writer's edit completes while THIS call is
                // still running — i.e. strictly between the read `existing`
                // came from and write_atomic_cas's commit-moment rename.
                std::fs::write(&path, "# Notes\n\nconcurrent edit\n").unwrap();
            }
            format!("{existing}\nmanaged: true\n")
        })
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            calls, 2,
            "expected exactly one retry after the injected race"
        );
        assert!(
            after.contains("concurrent edit"),
            "the concurrent edit must survive, not be silently discarded:\n{after}"
        );
        assert!(
            !after.contains("original\nmanaged: true"),
            "must not commit a version computed from the stale pre-race read:\n{after}"
        );
    }

    /// Same injected race, but on every single attempt: the retry budget
    /// must be bounded and the failure loud, never an infinite loop and
    /// never a silent partial write.
    #[test]
    fn write_atomic_cas_gives_up_loudly_under_sustained_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        std::fs::write(&path, "# Notes\n\nv0\n").unwrap();

        let mut calls = 0u32;
        let err = write_atomic_cas(&path, tmp.path(), |_existing| {
            calls += 1;
            // A change on every attempt, including the last: the file
            // never settles, so every recheck must find a mismatch.
            std::fs::write(&path, format!("# Notes\n\nv{calls}\n")).unwrap();
            "new content".to_string()
        })
        .unwrap_err();

        assert_eq!(calls, MAX_CAS_ATTEMPTS, "must not retry past the bound");
        assert!(
            format!("{err:#}").contains("changed concurrently"),
            "error must name the reason: {err:#}"
        );
        // No litter left behind by the abandoned attempts.
        let strays = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".pact-write"))
            .count();
        assert_eq!(strays, 0, "temp files left behind after giving up");
    }

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

    /// pact-m7j.9.11: `ensure_gitignore`/`ensure_gitattributes` used to
    /// compute their answer from a single read and write it with the plain,
    /// non-CAS `write_atomic` — the same read-modify-write shape pact-m7j.9.2
    /// fixed for `splice_block` after reproducing a concurrent hand-edit
    /// landing between the read and the rename and being silently,
    /// completely lost. Same injected race as
    /// `write_atomic_cas_never_commits_over_a_write_that_landed_mid_call`,
    /// but through the real `gitignore_content` closure `ensure_gitignore`
    /// actually uses, so this proves the wiring, not just the primitive.
    #[test]
    fn ensure_gitignore_does_not_discard_a_concurrent_hand_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        std::fs::write(&path, "target/\n").unwrap();

        let mut calls = 0u32;
        write_atomic_cas(&path, tmp.path(), |existing| {
            calls += 1;
            if calls == 1 {
                std::fs::write(&path, "target/\nnode_modules/\n").unwrap();
            }
            gitignore_content(existing)
        })
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            calls, 2,
            "expected exactly one retry after the injected race"
        );
        assert!(
            after.contains("node_modules/"),
            "the concurrent hand-edit must survive: {after}"
        );
        assert!(
            after.contains(RUNTIME_IGNORE_SENTINEL),
            "the pact rules must still be added: {after}"
        );
    }

    /// Same race, `ensure_gitattributes`'s side.
    #[test]
    fn ensure_gitattributes_does_not_discard_a_concurrent_hand_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitattributes");
        std::fs::write(&path, "*.bin binary\n").unwrap();

        let mut calls = 0u32;
        write_atomic_cas(&path, tmp.path(), |existing| {
            calls += 1;
            if calls == 1 {
                std::fs::write(&path, "*.bin binary\n*.psd binary\n").unwrap();
            }
            gitattributes_content(existing)
        })
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            calls, 2,
            "expected exactly one retry after the injected race"
        );
        assert!(
            after.contains("*.psd binary"),
            "the concurrent hand-edit must survive: {after}"
        );
        assert!(
            after.contains(EVENTS_LOG_PATH),
            "the pact rule must still be added: {after}"
        );
    }

    #[test]
    fn ensure_gitignore_creates_file_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_gitignore(tmp.path()).unwrap();

        let content = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        let rules: Vec<&str> = content
            .lines()
            .map(str::trim)
            .filter(|l| l.contains(".pact") && !l.starts_with('#'))
            .collect();
        // Deny everything under .pact/, then re-include the log. The ORDER is
        // significant to git, so this is a sequence comparison, not a set one.
        assert_eq!(rules, RUNTIME_IGNORE_RULES);
        assert!(
            !content
                .lines()
                .any(|l| l.trim().trim_end_matches('/') == ".pact"),
            "a broad .pact rule would swallow the event log:\n{content}"
        );
    }

    /// The whole point of pact-rnc.16: agent-written files under `.pact/`
    /// (e.g. `.pact/evidence/*`) must not need a new gitignore rule each.
    /// A repo initialised by an older pact carries a broad `.pact/` rule. Leaving
    /// it would keep the history lost for exactly the repos that have the most of
    /// it, so a re-run narrows it — in place, and without disturbing anything
    /// else in the file.
    #[test]
    fn ensure_gitignore_narrows_a_broad_rule_from_an_older_pact() {
        for original in [".pact/\n", ".pact\n", "target/\n.pact/\n*.log\n"] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(".gitignore");
            std::fs::write(&path, original).unwrap();

            ensure_gitignore(tmp.path()).unwrap();
            let after = std::fs::read_to_string(&path).unwrap();

            assert!(
                !after
                    .lines()
                    .any(|l| l.trim().trim_end_matches('/') == ".pact"),
                "the broad rule survived {original:?}:\n{after}"
            );
            for rule in RUNTIME_IGNORE_RULES {
                assert!(
                    after.lines().any(|l| l.trim() == *rule),
                    "missing {rule}:\n{after}"
                );
            }
            // Unrelated lines are not collateral.
            for kept in original
                .lines()
                .filter(|l| l.trim().trim_end_matches('/') != ".pact")
            {
                assert!(after.lines().any(|l| l == kept), "lost {kept:?}:\n{after}");
            }
        }
    }

    /// Already-narrow rules are a no-op, so `init` does not append a second copy.
    #[test]
    fn ensure_gitignore_leaves_narrow_rules_alone() {
        let original = "target/\n.pact/*\n!.pact/events.jsonl\n";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        std::fs::write(&path, original).unwrap();

        ensure_gitignore(tmp.path()).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// The event log must never end up ignored by anything `init` writes — the
    /// property every other assertion here is really about.
    #[test]
    fn ensure_gitignore_never_ignores_the_event_log() {
        for original in ["", "target/\n", ".pact/\n", ".pact/leases/\n"] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(".gitignore");
            if !original.is_empty() {
                std::fs::write(&path, original).unwrap();
            }
            ensure_gitignore(tmp.path()).unwrap();
            let after = std::fs::read_to_string(&path).unwrap_or_default();
            for line in after.lines().map(str::trim) {
                assert_ne!(line, ".pact");
                assert_ne!(line, ".pact/");
                assert_ne!(line, EVENTS_LOG_PATH);
            }
        }
    }

    #[test]
    fn ensure_gitattributes_adds_union_merge_once() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_gitattributes(tmp.path()).unwrap();
        ensure_gitattributes(tmp.path()).unwrap();

        let content = std::fs::read_to_string(tmp.path().join(".gitattributes")).unwrap();
        let hits: Vec<&str> = content
            .lines()
            .filter(|l| l.split_whitespace().next() == Some(EVENTS_LOG_PATH))
            .collect();
        assert_eq!(hits, [format!("{EVENTS_LOG_PATH} merge=union")]);
    }

    /// A deliberate choice of merge driver is the user's, not pact's.
    #[test]
    fn ensure_gitattributes_respects_an_existing_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitattributes");
        let original = format!("{EVENTS_LOG_PATH} merge=ours\n");
        std::fs::write(&path, &original).unwrap();

        ensure_gitattributes(tmp.path()).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
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
            "lease note IS the announcement",
            "pact agents --for",
            "--to-owner-of",
            "follows the file",
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

    /// The rule from pact-4zx: manage what is already there, invent nothing.
    /// A repo with no Gemini in it must not sprout a GEMINI.md.
    #[test]
    fn ensure_instruction_files_only_touches_files_that_already_exist() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("GEMINI.md"), "# Gemini\n").unwrap();

        let managed = ensure_instruction_files(tmp.path()).unwrap();

        assert_eq!(managed, vec![tmp.path().join("GEMINI.md")]);
        assert!(!tmp.path().join(".windsurfrules").exists());
        assert!(!tmp.path().join(".cursorrules").exists());
        assert!(!tmp.path().join(".github/copilot-instructions.md").exists());

        let after = std::fs::read_to_string(tmp.path().join("GEMINI.md")).unwrap();
        assert!(after.starts_with("# Gemini\n"), "prior content survives");
        assert!(after.contains(BEGIN_MARKER) && after.contains(END_MARKER));
    }

    /// The whole reason this is not one shared loop: a format that expands
    /// `@AGENTS.md` gets the import, one that does not gets prose — and
    /// *neither* gets a copy of the protocol, which would be a second thing
    /// for `is_current` to police and the drift the markers exist to prevent.
    #[test]
    fn instruction_blocks_reference_the_protocol_and_never_copy_it() {
        let with_import = pointer_block(true);
        let prose_only = pointer_block(false);

        assert!(with_import.contains(CLAUDE_IMPORT));
        assert!(!prose_only.contains(CLAUDE_IMPORT));
        for block in [&with_import, &prose_only] {
            assert!(block.contains("AGENTS.md"), "must name the source of truth");
            // A distinctive sentence from the protocol itself: if it ever shows
            // up here, someone turned the pointer into a copy.
            assert!(!block.contains("your agent identity comes from"));
        }
    }

    #[test]
    fn ensure_instruction_files_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".github")).unwrap();
        for f in [
            "GEMINI.md",
            ".github/copilot-instructions.md",
            ".clinerules",
        ] {
            std::fs::write(tmp.path().join(f), "# existing\n").unwrap();
        }

        ensure_instruction_files(tmp.path()).unwrap();
        let first: Vec<String> = [
            "GEMINI.md",
            ".github/copilot-instructions.md",
            ".clinerules",
        ]
        .iter()
        .map(|f| std::fs::read_to_string(tmp.path().join(f)).unwrap())
        .collect();

        ensure_instruction_files(tmp.path()).unwrap();
        let second: Vec<String> = [
            "GEMINI.md",
            ".github/copilot-instructions.md",
            ".clinerules",
        ]
        .iter()
        .map(|f| std::fs::read_to_string(tmp.path().join(f)).unwrap())
        .collect();

        assert_eq!(first, second);
        // Copilot CLI expands the import, VS Code's Copilot does not — that
        // file needs both halves or half of Copilot sees nothing.
        assert!(first[1].contains(CLAUDE_IMPORT));
        assert!(first[1].contains("Read `AGENTS.md`"));
        // .clinerules has no import mechanism at all: prose, no dangling link.
        assert!(!first[2].contains(CLAUDE_IMPORT));
    }

    /// `pact init` used to destroy the protocol it had just written whenever a
    /// target was a symlink to AGENTS.md: `is_file()` follows the link, so the
    /// pointer block was spliced through it into AGENTS.md itself, and the
    /// repo never recovered because every re-run repeated the sequence. The
    /// needle is a sentence from the protocol body, not the markers — a
    /// markers-only assertion passes while the content is gone, which is
    /// exactly what the wiped file looked like.
    #[cfg(unix)]
    #[test]
    fn a_target_symlinked_at_agents_md_is_not_written_through() {
        let tmp = tempfile::tempdir().unwrap();
        apply(tmp.path()).unwrap();
        std::os::unix::fs::symlink("AGENTS.md", tmp.path().join("GEMINI.md")).unwrap();
        // A real target alongside it: the guard must skip the alias, not the loop.
        std::fs::write(tmp.path().join(".windsurfrules"), "be nice\n").unwrap();

        let managed = ensure_instruction_files(tmp.path()).unwrap();

        assert_eq!(managed, vec![tmp.path().join(".windsurfrules")]);
        let agents = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert!(
            agents.contains("your agent identity comes from"),
            "the protocol was overwritten by the pointer block:\n{agents}"
        );
        assert!(is_current(tmp.path()).unwrap(), "init must converge");
        // ...and doctor must not then nag about the file it correctly skipped.
        assert!(stale_instruction_files(tmp.path()).unwrap().is_empty());
        assert!(
            managed_instruction_files(tmp.path()) == vec![tmp.path().join(".windsurfrules")],
            "an alias of AGENTS.md is not a separate managed file"
        );
    }

    /// pact-m7j.9.5: two OTHER targets aliased to each other (not to
    /// AGENTS.md) used to both get spliced independently — since they are the
    /// same underlying file, the second write clobbers the first's block, and
    /// `doctor` reported one stale right after `init` claimed to update both.
    /// `GEMINI.md` sorts first in `INSTRUCTION_TARGETS`, so it is the one kept;
    /// `.cursorrules` is the later alias, skipped rather than written.
    #[cfg(unix)]
    #[test]
    fn two_targets_aliased_to_each_other_are_treated_as_one_managed_file() {
        let tmp = tempfile::tempdir().unwrap();
        apply(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("shared.md"), "# shared\n").unwrap();
        std::os::unix::fs::symlink("shared.md", tmp.path().join("GEMINI.md")).unwrap();
        std::os::unix::fs::symlink("shared.md", tmp.path().join(".cursorrules")).unwrap();

        let managed = ensure_instruction_files(tmp.path()).unwrap();

        assert_eq!(
            managed,
            vec![tmp.path().join("GEMINI.md")],
            "only the earlier-listed alias should be written"
        );
        let shared = std::fs::read_to_string(tmp.path().join("shared.md")).unwrap();
        assert_eq!(
            shared.matches(BEGIN_MARKER).count(),
            1,
            "the shared file must carry exactly one managed block:\n{shared}"
        );
        // The acceptance property: doctor must not disagree with the init it
        // just ran.
        assert!(
            stale_instruction_files(tmp.path()).unwrap().is_empty(),
            "doctor must not flag a file init just wrote as stale"
        );
        assert_eq!(
            managed_instruction_files(tmp.path()),
            vec![tmp.path().join("GEMINI.md")]
        );
    }

    /// `.clinerules` is allowed to be a directory in newer Cline. Splicing a
    /// block into a directory path is an error, so it must be skipped rather
    /// than turn `pact init` into a failure for anyone using that layout.
    #[test]
    fn a_directory_shaped_target_is_skipped_not_a_hard_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".clinerules")).unwrap();

        assert!(ensure_instruction_files(tmp.path()).unwrap().is_empty());
        assert!(stale_instruction_files(tmp.path()).unwrap().is_empty());
        assert!(managed_instruction_files(tmp.path()).is_empty());
        assert!(tmp.path().join(".clinerules").is_dir());
    }

    #[test]
    fn stale_instruction_files_reports_missing_then_goes_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(stale_instruction_files(tmp.path()).unwrap().is_empty());

        std::fs::write(tmp.path().join(".windsurfrules"), "be nice\n").unwrap();
        let path = tmp.path().join(".windsurfrules");
        assert_eq!(
            stale_instruction_files(tmp.path()).unwrap(),
            vec![path.clone()]
        );

        ensure_instruction_files(tmp.path()).unwrap();
        assert!(stale_instruction_files(tmp.path()).unwrap().is_empty());

        // Edited by hand inside the markers: doctor must notice, exactly as it
        // does for AGENTS.md.
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("Read `AGENTS.md`", "Read AGENTS.md maybe");
        std::fs::write(&path, edited).unwrap();
        assert_eq!(stale_instruction_files(tmp.path()).unwrap(), vec![path]);
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
            .filter(|l| l.trim() == RUNTIME_IGNORE_SENTINEL)
            .count();
        assert_eq!(count, 1, "a second run appended a duplicate:\n{content}");
        assert!(content.contains("target/"));
    }
}
