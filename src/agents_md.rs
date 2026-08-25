//! Idempotent managed section in `AGENTS.md` teaching agents the pact
//! coordination protocol. Never touches content outside the markers.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// What every begin marker starts with. Matching is done on this PREFIX, not
/// on a whole marker, because the marker carries the block's hash and so is
/// not a fixed string — and because a repo written by a pact that predates the
/// hash has the bare `<!-- pact:begin -->`, which must still be found so
/// `pact init` can replace it.
pub const BEGIN_MARKER_PREFIX: &str = "<!-- pact:begin";
pub const END_MARKER: &str = "<!-- pact:end -->";

/// The begin marker pact writes today: the prefix plus the hash of the block
/// it is wrapping (pact-okz.1).
///
/// Self-identifying for the same reason bd's marker in the same file is
/// (`<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->`):
/// without it, "which version of the protocol was this fleet actually
/// following" is answerable only by git archaeology on this file. That is not
/// hypothetical — it produced a wrong analysis of whether agents message
/// voluntarily, because 223 messages from pact's own fleet turned out to
/// predate the protocol change that suppressed them and nothing in any
/// artifact said so.
pub fn begin_marker() -> String {
    format!(
        "{BEGIN_MARKER_PREFIX} hash:{} -->",
        block_hash(&managed_block())
    )
}

/// A short, stable identifier for one revision of the protocol text.
///
/// Truncated to 8 hex characters, which is an identity tag rather than an
/// integrity check — `is_current` still compares the block's full text, so a
/// collision would change nothing about staleness detection, only make two
/// eras share a label. The same length bd uses, for the same job.
pub fn block_hash(block: &str) -> String {
    // Reuses the event log's hash rather than introducing a second one, with a
    // fixed domain separator so a block and an event that happen to serialize
    // to the same bytes cannot produce the same value.
    crate::events::chain_hash_of("pact-protocol-block", block)[..8].to_string()
}

/// The hash of the block **actually present in `AGENTS.md`**, or `None` when
/// there is no readable managed block there.
///
/// Deliberately the file's block, not [`begin_marker`]'s: the question this
/// exists to answer is which protocol the agents in a run were reading, and a
/// repository that has not re-run `pact init` since an upgrade is following
/// the older text no matter what the binary would write. `Event::pact_version`
/// already records which binary ran.
///
/// Not memoized. A `OnceLock` would be wrong rather than merely unnecessary:
/// the test suite drives many repositories inside one process, so a global
/// cache would report the first repo's protocol for all of them. The read is a
/// few kilobytes, a handful of times per command.
pub fn current_block_hash(repo_root: &Path) -> Option<String> {
    let content = std::fs::read_to_string(repo_root.join("AGENTS.md")).ok()?;
    let (begin, end) = find_block_bounds(&content)?;
    let block = &content[begin..end];
    // Hash the BODY, not the wrapper: the marker contains the hash, so
    // including it would make the value depend on itself.
    let body_start = block.find("-->")? + "-->".len();
    let body = block[body_start..].trim_start_matches('\n');
    let body = body.strip_suffix('\n').unwrap_or(body);
    let body = body.strip_suffix(END_MARKER).unwrap_or(body);
    Some(block_hash(body))
}

/// The protocol block injected between the markers.
pub fn managed_block() -> String {
    r#"## pact coordination protocol

pact coordinates multiple coding agents working in this repository. Follow
this protocol whenever you touch shared files or hand off work to others.

- **Identity**: your agent identity comes from the `PACT_AGENT` environment
  variable (or `--agent <name>`). Set one before running pact commands; pact
  never guesses an identity. `pact whoami` shows the identity and paths it
  resolved.
- **Also export `BEADS_ACTOR=$PACT_AGENT`, once, in the same shell.** pact
  writes nothing to bd, so nothing pact runs can attribute your task tracking
  for you: `bd ready`/`bd update --claim`/`bd close` are yours alone. Without
  this they fall through to bd's next attribution tier — your shared checkout's
  `git user.name` — so a 15-agent fleet's entire task-tracking history can
  attribute to one identity while `.pact/events.jsonl` correctly shows sixteen.
  `pact whoami` prints the exact line to run.
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
  --note "running bd against the shared store"` before you run a tool that might
  write there. An agent that had correctly leased both source files it edited
  still corrupted the shared Beads store, because it read the protocol as being
  about editing files and a CLI wrote a second database behind it at exit 0.
  pact itself never writes to `.beads/`; the commands you run directly do.
- **If you are the ORCHESTRATOR, this file is addressed to you too.** You have no
  bead, no wave and no claim, so every rule here reads as somebody else's — and
  you are the participant with the broadest write access: shared skeletons,
  pre-wiring, merges, checkpoints. Lease the skeleton before you write it. On one
  20-agent run `pact audit --check commit-correlation` found 12 commits no hold
  covered and every one was the orchestrator's, breaking the rule it had written
  into all 16 workers' prompts — which all 16 followed. `--allow-main` excuses you
  from `--check topology`, not from holding leases. And read the handoffs for the
  beads your skeleton serves (`pact msg thread bead:<id>`) before you write it:
  they are addressed to a bead, not to you, so no inbox will hand them over.
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
  yourself finished, not after — but **commit before you release**. A lease
  released while the work is still uncommitted breaks the one binding the log
  exists to prove; measured on a 20-agent build, a fix landed 99 seconds after
  its author had already let the file go, and `pact audit --check
  commit-correlation` reports it as a commit nobody held a lease for.
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
  **Someone must have held it first.** pact resolves `--to-owner-of` through the
  record of who has leased the path, so a path nobody has ever leased has no
  owner to address and the send is refused outright. You cannot pre-address work
  that has not started — for that, name the agent with `--to`.
- **On exit 2, wait INSIDE the command: `pact lease acquire <path> --wait <dur>`.**
  It blocks until the path is free and returns the moment it is, so you never end
  your turn to wait. That matters more than it sounds: if you are a subagent, your
  process IS your turn loop, and ending a turn to wait for a notification is the
  same as exiting — nothing can re-enter you. Measured on one 12-agent fleet, seven
  agents took the old advice to "subscribe and pick up other work", four never
  resumed at all, and the three that did resumed nine hours later within fourteen
  seconds of each other, because a human woke the parent session. One of them was
  holding four finished, tested, committed fixes.
  **`pact watch add <path>` is still right when you genuinely have other work
  first** and will still be running to receive the diff. It is not a way to wait.
  **Never poll by re-running the command yourself.** That spends a turn per
  attempt and is what `pact audit --check retry-storm` counts: one fleet retried
  every 15 seconds, 33 times, against a median 355 seconds of remaining hold, and
  24 refusals in that run came from agents that had ALREADY subscribed and polled
  anyway.
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
- **Use a file for anything longer than a sentence**: `--body-file <path>`.
  Quotes, backslashes and aligned tables do not survive a shell, and handing
  over an API is exactly that kind of content.
- **Read and reply in the same thread**: `pact msg inbox` lists one line per
  message; `pact msg read <id>` shows one in full together with its whole
  thread. Reply with `pact msg send --to <sender> --thread <id> "..."` — a
  reply sent without `--thread` starts a new thread, and the exchange stops
  being readable as one conversation.
- **Confirm, don't re-send**: `pact msg sent` shows what you sent and whether
  the recipient has read it. If you are unsure a message went out, check
  there — a blind re-send is how a peer's inbox fills with duplicates.
- **Subscribe to the interfaces you depend on but do not own.** At task start,
  `pact watch add <path>` (a file, or a directory for everything under it) for
  every file whose contract your work assumes. When its holder releases it,
  pact sends you the diff of what they changed — automatically, as part of
  their `lease release`. Nobody has to remember to tell you. This exists
  because they demonstrably will not: across three fleet runs since the
  protocol started reserving messages for what needs something back, 28 agents
  sent 4 messages between them, and the one that mattered was the only reason a
  runtime panic did not ship.
  **In a worktree fleet a notice is a contract notice, not a code delivery.** It
  names the branch the change is on; that change cannot appear in your tree until
  the branch merges and you merge that. Read the diff for what the contract now
  says and keep going — waiting for the file to change under you is waiting for
  something that structurally cannot happen.
- **Read your inbox at task start AND before your final commit.** The first
  tells you what changed under you before you plan; the second catches the
  interface change that landed while you were working, which is exactly when it
  is cheapest to absorb and most expensive to miss.
- **If you act on a message, mark it read.** `pact msg read <id>` is the only
  thing that tells the sender their warning landed; act on one without it and
  their `pact msg sent` says "undelivered" forever, which is indistinguishable
  from being ignored. Across two fleet builds, three of four messages were
  never acknowledged by the agent they were addressed to — including one that
  prevented a runtime panic. `pact audit --export` lists the stragglers.
- **A red shared branch is NEVER a reason to hold a finished merge.**
  `pact merge --verify` asks whether YOUR merge added a failure, not whether the
  branch is green. Arriving to a branch that is already failing for somebody
  else's reason, it lands your work anyway, says so, and releases the mutex; only
  a failure your merge introduced is reverted, and only then does it keep the
  mutex. So merge when your work is done and proven, and let pact decide which of
  those two happened.
  This rule is here — in the block `pact init` syncs into every repository —
  rather than in one fleet's own notes, because that is where it was and it cost
  a run. Four agents in one 12-agent fleet independently held finished, tested,
  committed work off a red master, each citing the mechanic correctly: *"merging
  now would falsely go red due to their unrelated unfixed bug"*. They were not
  defying the rule; the rule did not exist yet where they could read it. It was
  written 38 minutes after the first of them parked, and reached the NEXT
  cohort's spawn prompt only. One of those four was holding four finished fixes,
  two of them repaired regressions.
- **Gates are beads, and they are visible in `bd` like any other.** Before you
  claim into a new wave, check that the prior wave's gates have closed. pact will
  not stop you — no acquire is ever refused on gate grounds — but `pact audit
  --check gate-order` reads the ledger either way, and a start it finds ahead of a
  gate is a question somebody will ask afterwards.
- **Read your inheritance before you start a claimed bead**: `pact msg thread
  bead:<id>`. Whoever finished what yours depends on may have left findings there
  — addressed to the bead rather than to you, because when they wrote it you did
  not exist yet. It is usually the cheapest thing you will read all session.
- **When you close a bead that has dependents, send a handoff**: `pact handoff
  <bead> --confidence high|medium|low --findings "<what you found>"`. Findings you
  would want waiting for you. It never blocks and nothing waits on it; a bead with
  nothing worth saying should send nothing.
- **Orient with `pact log`**: one chronological feed of who leased what and
  who said what. Read it when you join, and when you need to know whether a
  peer is still moving.
- **The coordination logs are committed from the MAIN checkout, not from your
  worktree.** `.pact/events.jsonl` and `.pact/messages.jsonl` are the two things pact
  stores that it cannot derive from anything else — who held what, and what agents
  said to each other — so they do belong in git. But under the default shared scope
  every worktree resolves state to the main checkout, so from a worktree your copy of
  those files is a stale tracked snapshot and `git add` finds nothing to stage.
  **If you are working in a worktree, do not try to commit them.** Whoever owns the
  main checkout — usually the orchestrator — commits them for the whole fleet, and a
  missed one is self-healing on the next commit.
  This sentence used to say "commit both when you commit your work", and 35 agents in
  one run each spent time discovering that it is impossible to follow from where they
  were standing; nine reported it independently and unprompted.
  `.pact/leases/`, `.pact/waits/` and `.pact/read/` stay local everywhere — live
  runtime state and per-machine read positions, and committing those would have you
  fighting over peers' in-flight claims and inboxes.
- **Sign your commits with your agent name**: `git commit --trailer
  Pact-Agent=$PACT_AGENT`. Every agent in a fleet commits under the same git
  identity, so `git log` cannot say which of you made a change — and without
  that, `pact audit --check commit-correlation` can only ask whether ANYONE held
  a path when a commit landed, never whether the agent that made it did. Measured:
  one agent working with no leases at all had its worst commit (five files, all of
  them leased by compliant peers at that moment) pass the check clean, because a
  hold existed. The better everyone else behaves, the better an unleased commit
  hides. One flag makes it visible.
- **Three git commands take a target you did not name — do not use them in a
  shared checkout.** A fleet shares one index and one HEAD, and each of these
  resolves against whatever the checkout is at the instant it runs rather than
  against the paths you own. All three were paid for here:
  - `git commit` with no pathspec commits the whole INDEX, so it sweeps in
    whatever a peer had staged. One run put another agent's staged deletion into
    an unrelated commit. Always `git commit -- <explicit paths>`.
  - `git commit --only <path>` fails SILENTLY when the path is untracked — it
    prints `did not match any file(s)` and exits non-zero while a surrounding
    green build reports success. `git add` the file first.
  - `git commit --amend` amends whatever HEAD is NOW, which in a fleet is
    routinely a peer's commit landed seconds ago. One run rewrote a peer's
    message and folded two agents' work into one mislabelled commit. There is no
    pathspec that protects you: the target is implicit. If you need to fix a
    commit, add a follow-up commit instead.
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
    let block = format!("{}\n{body}{END_MARKER}\n", begin_marker());

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
    let mut sentinel_at: Option<usize> = None;

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
            sentinel_at = Some(out.len());
        }
        out.push(line.to_string());
    }

    // Which committed files this file already re-includes. Checked PER PATH, because
    // the sentinel only proves the DENY rule is present — it says nothing about the
    // negations beside it, and an older pact wrote a deny with just one.
    //
    // This is the "attribute-but-ignored" bug from finding 1, and it was ours: the
    // gitattributes half was made per-path when the message store became committed, and
    // this half was left keyed on the sentinel. So a repo initialised before 0.9 got the
    // `merge=union` attribute for messages.jsonl and never got the negation — a file
    // simultaneously gitignored and carrying a merge driver, which is exactly the state
    // the field audit found.
    let missing: Vec<&str> = COMMITTED_APPEND_ONLY
        .iter()
        .copied()
        .filter(|path| {
            let negation = format!("!{path}");
            !out.iter().any(|l| l.trim() == negation)
        })
        .collect();

    if narrowed {
        // Fall through and write: narrowing already emits the complete rule set.
    } else if already && missing.is_empty() {
        // Byte-for-byte what was already there — write_atomic_cas still
        // renames it, same as splice_block does on an already-current
        // AGENTS.md, rather than adding a second "is a write actually
        // needed" branch here to skip it.
        return existing.to_string();
    } else if already {
        // Insert the missing negations directly AFTER the deny rule rather than at the
        // end of the file. gitignore is last-match-wins, so a negation must follow the
        // rule it overrides — and appending at the end would put it after any unrelated
        // rule the user added below, where a later `.pact/**` of their own would silently
        // win.
        let at = sentinel_at.map_or(out.len(), |i| i + 1);
        for (offset, path) in missing.iter().enumerate() {
            out.insert(at + offset, format!("!{path}"));
        }
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
const RUNTIME_IGNORE_RULES: &[&str] = &[
    ".pact/*",
    "!.pact/events.jsonl",
    // Committable for the same reason the event log is: it is the record of what
    // agents said to each other, and it cannot be derived from anything else. The
    // read CURSORS stay local — a read position is per-machine — which is what keeps
    // "who said what" shared without making "who has read it" a merge conflict.
    "!.pact/messages.jsonl",
    // The third, and the only one that is not append-only (pact-e7d). It is a
    // SNAPSHOT of the dependency graph `pact plan lint` last accepted, rewritten
    // whole each time — so it carries no history and needs no merge driver, which
    // is why it is listed here but absent from `APPEND_ONLY_PATHS`.
    //
    // Committed for the reason the two above are: something has to be able to read
    // it after the run. `pact audit --check handoff-coverage` asks which closed
    // beads had dependents and said nothing to them, and that question cannot be
    // answered from a file the clone did not get. The orchestrator's own input
    // manifest stays a build artifact, exactly as docs/plan.md says — this is
    // pact's linted copy of it, not the manifest.
    "!.pact/plan.json",
];

/// The line that decides whether the rules are already present. Keyed on the
/// deny line: a repo with `.pact/*` has been through this.
const RUNTIME_IGNORE_SENTINEL: &str = ".pact/*";

const RUNTIME_IGNORE_COMMENT_1: &str =
    "# Everything pact or an agent writes under .pact/ is local runtime state,";
const RUNTIME_IGNORE_COMMENT_2: &str =
    "# EXCEPT the files below, which are history or graph and belong in git.";

/// `.pact/events.jsonl`: the lease-event log, and the first file under `.pact/`
/// that belonged in git.
pub const EVENTS_LOG_PATH: &str = ".pact/events.jsonl";

/// `.pact/messages.jsonl`: the message store, committed since 0.9.0.
pub const MESSAGES_STORE_PATH: &str = ".pact/messages.jsonl";

/// Every append-only file pact commits, and therefore every path that needs a
/// union merge driver.
///
/// A list rather than a constant, because there are two of them now and a second
/// hand-maintained copy of "which files does pact commit" is how one of them gets
/// forgotten — which is exactly what happened when the message store became
/// committed without a matching merge rule.
pub const COMMITTED_APPEND_ONLY: &[&str] = &[EVENTS_LOG_PATH, MESSAGES_STORE_PATH];

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

/// Does `existing` already say something about `path`?
///
/// Per PATH, not per file: the old check returned early if `.pact/events.jsonl`
/// appeared anywhere, which meant every repository initialised before 0.9.0 —
/// i.e. every existing one — would never receive the rule for the message store,
/// no matter how many times `pact init` was re-run. The bug only shows up on
/// upgrade, which is the case a sentinel check is least likely to be tested
/// against.
fn mentions_path(existing: &str, path: &str) -> bool {
    existing
        .lines()
        .any(|l| l.split_whitespace().next() == Some(path))
}

/// Pure computation half of [`ensure_gitattributes`] — see
/// [`gitignore_content`]'s doc comment for why this is split out.
fn gitattributes_content(existing: &str) -> String {
    let missing: Vec<&str> = COMMITTED_APPEND_ONLY
        .iter()
        .copied()
        .filter(|p| !mentions_path(existing, p))
        .collect();
    if missing.is_empty() {
        return existing.to_string();
    }

    let mut out: Vec<String> = existing.lines().map(str::to_string).collect();
    if !out.is_empty() {
        out.push(String::new());
    }
    out.push("# pact: these logs are append-only, so a merge keeps BOTH sides".to_string());
    out.push("# rather than stopping for a human who has nothing to decide.".to_string());
    out.extend(missing.iter().map(|p| format!("{p} merge=union")));

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
    Ok(existing[begin..end] == format!("{}\n{body}{END_MARKER}\n", begin_marker()))
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
    let begin = content.find(BEGIN_MARKER_PREFIX)?;
    // Past the marker's own `-->`, wherever the hash puts it — and tolerant of
    // the bare `<!-- pact:begin -->` a pre-hash pact wrote.
    let after_begin = begin + content[begin..].find("-->")? + "-->".len();
    let end_rel = content[after_begin..].find(END_MARKER)?;
    let mut end = after_begin + end_rel + END_MARKER.len();
    // Consume one trailing newline after the end marker too, since `block`
    // already supplies its own — otherwise re-applying doubles it.
    if content[end..].starts_with('\n') {
        end += 1;
    }
    Some((begin, end))
}

/// Heading text (leading `#`s and whitespace stripped) mapped to every
/// 1-based line number it appears on, for headings appearing more than once
/// OUTSIDE pact's own managed block (pact-juz.3).
///
/// Not a pact bug when this returns anything — it means some OTHER tool
/// wrote its own section into this file more than once, unaware of the
/// other (e.g. `bd init` and `bd setup codex` each writing their own
/// independent "Quick Reference"). Confirmed by direct inspection that
/// pact's own `agents_md.rs` never writes that heading text at all — this
/// exists because pact is already the tool walking and understanding this
/// exact file, not because pact caused what it finds. Matched on text alone,
/// not heading level, since the real duplication observed in the field used
/// two different levels (`## Quick Reference` and `### Quick Reference`) for
/// the identical wording.
pub fn duplicated_headings_outside_managed_block(content: &str) -> Vec<(String, Vec<usize>)> {
    let managed = find_block_bounds(content);
    let mut by_text: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    let mut offset = 0usize;
    for (i, line) in content.lines().enumerate() {
        let start = offset;
        offset += line.len() + 1;
        if managed.is_some_and(|(b, e)| start >= b && start < e) {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            let text = trimmed.trim_start_matches('#').trim();
            if !text.is_empty() {
                by_text.entry(text.to_string()).or_default().push(i + 1);
            }
        }
    }
    by_text
        .into_iter()
        .filter(|(_, lines)| lines.len() > 1)
        .collect()
}

/// The nearest `<!-- ... -->` comment at or before 1-based `line`, if any —
/// context for [`duplicated_headings_outside_managed_block`]'s doctor check,
/// so a human can tell which two tools wrote the duplicates without
/// grepping by hand.
pub fn nearest_preceding_marker(content: &str, line: usize) -> Option<String> {
    let preceding: Vec<&str> = content.lines().take(line).collect();
    preceding
        .iter()
        .rev()
        .find(|l| l.trim_start().starts_with("<!--"))
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    /// The two lists that decide a file's fate must agree: anything pact COMMITS
    /// needs a `!` negation in the ignore rules and a union merge driver. They were
    /// maintained by hand and drifted exactly once — the message store became
    /// committed with no merge rule, so the per-worktree fleet pattern
    /// docs/fleet-patterns.md recommends would have conflicted on every wave.
    #[test]
    fn every_committed_file_is_both_un_ignored_and_union_merged() {
        for path in COMMITTED_APPEND_ONLY {
            assert!(
                RUNTIME_IGNORE_RULES.contains(&format!("!{path}").as_str()),
                "{path} is committed but has no `!` negation in RUNTIME_IGNORE_RULES, \
                 so `pact init` would write a .gitignore that hides it"
            );
            let attrs = gitattributes_content("");
            assert!(
                attrs.lines().any(|l| l == format!("{path} merge=union")),
                "{path} is committed and append-only but gets no union merge driver, \
                 so two branches appending to it conflict on every merge:\n{attrs}"
            );
            // AND on the upgrade path, which is where this guard was blind: it only ever
            // asked about a fresh file, so the asymmetry between the per-path
            // gitattributes half and the sentinel-keyed gitignore half survived it
            // (finding 1). Every committed path must be un-ignored on a repo that
            // already carries the deny rule with only SOME of the negations.
            let stale = format!(".pact/*\n!{}\n", COMMITTED_APPEND_ONLY[0]);
            let upgraded = gitignore_content(&stale);
            assert!(
                upgraded.lines().any(|l| l.trim() == format!("!{path}")),
                "{path} stays ignored when re-initialising a repo that predates it:\n{upgraded}"
            );
        }
    }

    /// THE UPGRADE PATH FOR THE IGNORE RULES, which is where the drift guard below was
    /// blind (pact-83r.2 / finding 1).
    ///
    /// A repo initialised before 0.9 has `.pact/*` plus a single `!.pact/events.jsonl`.
    /// The sentinel `.pact/*` is present, so the old code returned the file UNCHANGED and
    /// the message store never got its negation — while the gitattributes half, made
    /// per-path at the same time, happily added `messages.jsonl merge=union`. The result
    /// is a file simultaneously gitignored and carrying a merge driver, which is the state
    /// the field audit found in a real repository.
    #[test]
    fn re_init_adds_a_missing_negation_to_a_pre_0_9_ignore_rule() {
        let existing = "/target\n.pact/*\n!.pact/events.jsonl\ntmp/\n";
        let out = gitignore_content(existing);
        for path in COMMITTED_APPEND_ONLY {
            assert!(
                out.lines().any(|l| l.trim() == format!("!{path}")),
                "{path} was left ignored by a re-init:\n{out}"
            );
        }
        // The negation must come AFTER the deny rule it overrides — gitignore is
        // last-match-wins, so order is not cosmetic here.
        let deny = out
            .lines()
            .position(|l| l.trim() == RUNTIME_IGNORE_SENTINEL)
            .expect("deny rule kept");
        for path in COMMITTED_APPEND_ONLY {
            let at = out
                .lines()
                .position(|l| l.trim() == format!("!{path}"))
                .expect("negation present");
            assert!(at > deny, "!{path} precedes the rule it overrides:\n{out}");
        }
        // Unrelated rules survive untouched.
        for kept in ["/target", "tmp/"] {
            assert!(out.lines().any(|l| l == kept), "lost {kept}:\n{out}");
        }
        // Idempotent once complete.
        assert_eq!(gitignore_content(&out), out);
    }

    /// The upgrade path, which a sentinel check gets wrong by construction: a
    /// repository whose .gitattributes already names the event log must still
    /// receive the rule for the message store.
    #[test]
    fn gitattributes_adds_a_missing_path_to_a_file_that_already_has_the_other() {
        let existing = format!("{EVENTS_LOG_PATH} merge=union\n");
        let out = gitattributes_content(&existing);
        assert!(
            out.lines()
                .any(|l| l == format!("{MESSAGES_STORE_PATH} merge=union")),
            "an existing events.jsonl rule suppressed the messages.jsonl one:\n{out}"
        );
        // And it must not duplicate the one that was already there.
        let events = out
            .lines()
            .filter(|l| l.split_whitespace().next() == Some(EVENTS_LOG_PATH))
            .count();
        assert_eq!(events, 1, "duplicated the existing rule:\n{out}");

        // Idempotent once both are present.
        assert_eq!(gitattributes_content(&out), out);
    }

    /// A hand-chosen merge driver stays the user's call, per path.
    #[test]
    fn gitattributes_leaves_a_deliberate_merge_driver_alone() {
        let existing = format!("{MESSAGES_STORE_PATH} merge=ours\n");
        let out = gitattributes_content(&existing);
        assert!(
            out.lines()
                .any(|l| l == format!("{MESSAGES_STORE_PATH} merge=ours")),
            "overrode a deliberate choice:\n{out}"
        );
        assert!(
            !out.lines()
                .any(|l| l == format!("{MESSAGES_STORE_PATH} merge=union")),
            "added a second, contradicting rule:\n{out}"
        );
        // The path it says nothing about still gets its rule.
        assert!(out
            .lines()
            .any(|l| l == format!("{EVENTS_LOG_PATH} merge=union")));
    }

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
        assert!(content.starts_with(BEGIN_MARKER_PREFIX));
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
        assert!(after.contains(BEGIN_MARKER_PREFIX));
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
            "# Before\n\n<!-- pact:begin -->\nstale content that should be replaced\n{END_MARKER}\n\n# After\n"
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
        // A COMPLETE rule set, which is what "already narrow" has to mean since 0.9.0.
        // This test used to start from `.pact/*` plus only `!.pact/events.jsonl` and
        // assert the file came back byte-identical — which is precisely the expectation
        // that let the message store stay ignored forever on any repo that had been
        // through `pact init` before it existed (finding 1). A file missing one of the
        // two negations is not narrow, it is incomplete, and re-running init is the only
        // thing that will ever fix it.
        let original = format!(
            "target/\n{}\n{}\n",
            RUNTIME_IGNORE_SENTINEL,
            COMMITTED_APPEND_ONLY
                .iter()
                .map(|p| format!("!{p}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        std::fs::write(&path, &original).unwrap();

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

        // The deliberate choice for THAT path survives, which is what this test is
        // about. It used to assert the whole file came back byte-identical, which
        // quietly encoded the sentinel bug rather than the intent: "leave this path
        // alone" and "add nothing at all" are different promises, and only the first
        // is one pact should keep. A repo carrying a hand-picked driver for the event
        // log still needs a rule for the message store.
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after
                .lines()
                .any(|l| l == format!("{EVENTS_LOG_PATH} merge=ours")),
            "{after}"
        );
        assert!(
            !after
                .lines()
                .any(|l| l == format!("{EVENTS_LOG_PATH} merge=union")),
            "added a rule contradicting the one already there:\n{after}"
        );
        assert!(
            after
                .lines()
                .any(|l| l == format!("{MESSAGES_STORE_PATH} merge=union")),
            "the message store was left without a merge rule:\n{after}"
        );
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

    /// pact-juz.3: reproduces the exact shape found in the field — two
    /// SEPARATE tools (`bd init`, `bd setup codex`) each writing their own
    /// "Quick Reference" heading at a different level, unaware of the other,
    /// entirely before pact's own managed block. Matched on text, not level,
    /// since that is exactly what differed in the real case.
    /// pact-okz.1: the marker identifies which revision of the protocol it
    /// wraps, the way bd's marker in the same file already does.
    #[test]
    fn the_begin_marker_carries_the_blocks_hash() {
        let marker = begin_marker();
        assert!(marker.starts_with(BEGIN_MARKER_PREFIX), "{marker}");
        assert!(marker.ends_with(" -->"), "{marker}");
        let hash = block_hash(&managed_block());
        assert_eq!(hash.len(), 8, "short enough to read in a marker: {hash}");
        assert!(marker.contains(&format!("hash:{hash}")), "{marker}");
    }

    /// Two different protocol texts must not share a label, or the field
    /// cannot answer the question it exists for.
    #[test]
    fn a_changed_block_changes_its_hash() {
        let a = block_hash("## protocol\n\n- do the thing\n");
        let b = block_hash("## protocol\n\n- do the OTHER thing\n");
        assert_ne!(a, b);
        assert_eq!(
            a,
            block_hash("## protocol\n\n- do the thing\n"),
            "and is stable"
        );
    }

    /// The compatibility case that decides whether this ships safely: every
    /// repository in existence has the bare `<!-- pact:begin -->`, and
    /// `pact init` must still find and replace it rather than treating the
    /// file as unmarked and appending a SECOND block.
    #[test]
    fn a_pre_hash_marker_is_still_found_and_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let path = root.join("AGENTS.md");
        std::fs::write(
            &path,
            format!("# House rules\n\n<!-- pact:begin -->\nold protocol text\n{END_MARKER}\n\n# After\n"),
        )
        .unwrap();

        // Found, despite carrying no hash.
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            find_block_bounds(&content).is_some(),
            "a pre-hash marker must be found"
        );
        // And it is not "current", so `init` rewrites rather than skipping.
        assert!(!has_current_block(&path, &managed_block()).unwrap());

        apply(root).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches(BEGIN_MARKER_PREFIX).count(),
            1,
            "replaced in place, never appended alongside:\n{after}"
        );
        assert!(after.starts_with("# House rules"), "{after}");
        assert!(
            after.contains("# After"),
            "content after the block survives:\n{after}"
        );
        assert!(!after.contains("old protocol text"), "{after}");
        assert!(has_current_block(&path, &managed_block()).unwrap());
    }

    /// The hash recorded is the one in the FILE, not the one this binary
    /// would write — a repo that has not re-run `pact init` since an upgrade
    /// is still following the older text, and that difference is the whole
    /// point of the field.
    #[test]
    fn current_block_hash_reports_the_file_not_the_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(current_block_hash(root), None, "no AGENTS.md is no era");

        std::fs::write(
            root.join("AGENTS.md"),
            format!("<!-- pact:begin -->\nan older protocol\n{END_MARKER}\n"),
        )
        .unwrap();
        let stale = current_block_hash(root).expect("a block is present");
        assert_ne!(
            stale,
            block_hash(&managed_block()),
            "a stale block must not report this binary's hash"
        );

        apply(root).unwrap();
        assert_eq!(
            current_block_hash(root).as_deref(),
            Some(block_hash(&managed_block()).as_str()),
            "after init it matches what the binary writes"
        );
    }

    #[test]
    fn duplicated_headings_outside_the_managed_block_are_found() {
        let content = "\
# Agent Instructions

## Quick Reference

bd ready

<!-- BEGIN BEADS INTEGRATION -->
## Beads Issue Tracker

### Quick Reference

bd ready
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP -->
## Beads Issue Tracker

### Quick Reference
<!-- END BEADS CODEX SETUP -->
";
        let found = duplicated_headings_outside_managed_block(content);
        let texts: Vec<&str> = found.iter().map(|(t, _)| t.as_str()).collect();
        assert!(texts.contains(&"Quick Reference"), "{found:?}");
        assert!(texts.contains(&"Beads Issue Tracker"), "{found:?}");

        let (_, quick_ref_lines) = found.iter().find(|(t, _)| t == "Quick Reference").unwrap();
        assert_eq!(
            quick_ref_lines.len(),
            3,
            "all three occurrences, regardless of heading level: {quick_ref_lines:?}"
        );

        assert_eq!(
            nearest_preceding_marker(content, quick_ref_lines[1]).as_deref(),
            Some("<!-- BEGIN BEADS INTEGRATION -->"),
            "must name which marked block the second occurrence sits inside"
        );
    }

    /// pact's own managed block is exactly what `is_current` already
    /// polices — this check must never re-flag pact's own heading, which
    /// would be a duplicate warning about a duplicate that doesn't exist.
    #[test]
    fn a_file_with_only_pacts_own_block_reports_no_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        apply(tmp.path()).unwrap();
        let content = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert!(
            duplicated_headings_outside_managed_block(&content).is_empty(),
            "pact's own freshly-applied block must never trip this check"
        );
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
        assert!(after.contains(BEGIN_MARKER_PREFIX) && after.contains(END_MARKER));
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
            shared.matches(BEGIN_MARKER_PREFIX).count(),
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

#[cfg(test)]
mod x16_tests {
    use super::*;

    /// pact-x16.1 and pact-x16.2: two rules that cost a fleet because they lived
    /// somewhere agents could not read them.
    ///
    /// Both are asserted against the SYNCED block rather than against any
    /// document, because "where it lives" is the entire finding in both cases.
    /// The red-master rule was in one repository's annex, written 38 minutes
    /// after four agents had already parked finished work off a red master and
    /// reaching only the next cohort's spawn prompt. The wait advice was in a
    /// refusal payload that told a subagent to do something a subagent cannot do.
    /// A fix that landed in docs/ would reproduce both.
    #[test]
    fn the_synced_block_carries_the_rules_that_cost_a_fleet() {
        let block = managed_block();

        // pact-x16.1: what --verify actually asks, and the conclusion an agent
        // must not draw from a red branch.
        assert!(
            block.contains("added a failure"),
            "an agent has to know --verify asks whether ITS merge broke something"
        );
        assert!(
            block.contains("NEVER a reason to hold"),
            "four agents reasoned their way to the opposite; the block must \
             foreclose it"
        );

        // pact-x16.2: the wait that a subagent can actually execute, and the
        // absence of the advice it cannot.
        assert!(
            block.contains("--wait"),
            "the only form of waiting available to a subagent must be the one \
             the block names"
        );
        assert!(
            !block.contains("pick up\nother ready work") && !block.contains("do not poll."),
            "advice with no terminator must not survive a rewrite of the advice"
        );
    }
}
