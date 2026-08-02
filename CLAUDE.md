# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`pact` is a single Rust binary that coordinates multiple coding agents working
on one repository: it injects a coordination protocol into `AGENTS.md`, hands
out advisory file leases, and passes threaded messages between agents by
shelling out to the Beads CLI (`bd`).

## Build & test

```bash
mise run check    # fmt-check + clippy -D warnings + test — the CI gate
mise run build    # cargo build --features ui
mise run test     # cargo test --features ui
mise run install  # cargo install --path . --force --features ui  (CURRENT build on PATH)
```

Every mise task builds with `--features ui`, so `pact ui` exists in what you
build, test and install. `ui` is **not** a default Cargo feature — a plain
`cargo build` leaves ratatui out, and CI runs clippy and test both ways so that
dependency-light build stays guarded. `pact --version` prints the enabled
features, which is the fast answer to `unrecognized subcommand 'ui'`.

Run a single test: `cargo test <substring>`, e.g. `cargo test lease::tests::renew`.

Tests live in three places: unit tests inside each module, plus `tests/lease.rs`
and `tests/cli.rs`, which drive the **real binary** via `env!("CARGO_BIN_EXE_pact")`.
CLI behaviour belongs in `tests/cli.rs` — a batch of bugs once shipped precisely
because the CLI layer had no end-to-end coverage.

**After changing anything, `mise run install` before using `pact` by name.** A
stale binary on `PATH` silently regressed `AGENTS.md` and `.gitignore` once,
because `pact init` from an old build rewrites both with its old rules.

## Architecture

`src/main.rs` is CLI parsing and dispatch only; every subcommand delegates to a
module. `lease.rs` (advisory locks), `msg.rs` (messaging over bd), `beads.rs`
(the only place that shells out to `bd`), `agents.rs` (who is active),
`events.rs` (the lease-event log behind `pact log`), `agents_md.rs` (the managed
protocol block), `doctor.rs`, `identity.rs`, `output.rs`, `repo.rs`, and
`tui.rs` + `mascot.rs` (the `pact ui` dashboard).

Invariants worth knowing, because each was broken at least once:

- **Never `println!`.** All output goes through `output::line` / `output::warn`.
  A bare `println!` panics on a closed pipe (`… | head -1`) *after* the side
  effect has landed, so a command that succeeded reports failure — which made
  agents re-send messages they had already sent.
- **Exit codes are API**, documented in README: `0` ok, `1` generic, `2` lease
  held by another agent, `3` no `bd` on PATH, `4` not in a git repo. Raise them
  with `output::exit_with`; `main` maps them via `output::code_for`.
- **A question must not mutate.** Read-only paths use `repo::pact_dir_path`
  (does not create `.pact/`) and `lease::peek` (does not garbage-collect).
  `lease::list` *does* sweep expired locks — that is `lease ls`'s documented job.
- **Never touch the Beads DB directly** — always shell out via `beads.rs`. Read
  state lives in bd labels (`read-by-<agent>`), not in a local file, so a sender
  can see acknowledgement.
- **`agents_md::managed_block()` is the protocol text.** `is_current()` compares
  against it, so editing the text makes every repo report stale until `pact
  init` — that is the freshness check working, not a bug.
- **`tui.rs`: `tab_rects` is shared by rendering and mouse hit-testing** so the
  two cannot drift; widening a tab label without it breaks clicks. The event
  loop's poll timeout is `min(data-refresh remaining, next animation frame)` —
  shortening it naively spawns a `bd` subprocess ~10×/second.

Deeper detail lives in `docs/`: `architecture.md`, `leases.md`, `messaging.md`,
`tui.md`.

<!-- pact:begin -->
<!-- Mirrored from AGENTS.md. `pact init` only manages AGENTS.md today, so if
     the protocol text changes this copy must be updated by hand (see the bead
     about `pact init` managing every agent-instruction file present). -->
## pact coordination protocol

pact coordinates multiple coding agents working in this repository. Follow
this protocol whenever you touch shared files or hand off work to others.

- **Identity**: your agent identity comes from the `PACT_AGENT` environment
  variable (or `--agent <name>`). Set one before running pact commands; pact
  never guesses an identity. `pact whoami` shows the identity and paths it
  resolved.
- **Announce intent before you research, not just before you write.** Your
  first pact commands come *before* you read the first file: `pact msg inbox`,
  then `pact msg send --to <peer-or-human>` saying what you are about to work
  on, then `pact lease acquire <path> --note "<what>"` for the files you expect
  to own. Do it even if you will only be reading for the next ten minutes. Why:
  a peer planning against the same file can renegotiate now instead of at the
  end, when both plans are sunk cost — and a fleet that has announced nothing
  looks exactly like a fleet that crashed on startup.
- **Ownership, and its one carve-out, stated together**: lease every file you
  edit that another agent might also touch, and release it when done. The
  single exception is a file that is yours alone by assignment (your own
  evidence log, your own scratch dir) — nobody else writes it, so it needs no
  lease. Anything else: lease it. Leases are advisory, not enforced by the
  filesystem; respect them anyway.
- **Keep a lease alive, then let it all go**: `pact lease renew <path>`
  refreshes the TTL — a long task must not outlive its lease. `pact lease
  release <path>` frees one file, `pact lease release --all` frees everything
  you hold in a single call, so nothing gets half-forgotten. Release before
  you report yourself finished, not after.
- **Announce contract changes**: if you change an API, schema, CLI flag, or
  any other contract another agent depends on, message them:
  `pact msg send --to <agent> "what changed and why"`. Check the recipient
  exists with `pact agents` first — a mistyped name sends into the void. One
  decision that affects several agents goes out as ONE message: repeat `--to`
  and they all land in a single thread anyone can read and reply into.
- **Use a file for anything longer than a sentence**: `--body-file <path>`, or
  `--body-file -` for stdin. Quotes, backslashes and aligned tables do not
  survive a shell, and handing over an API is exactly that kind of content.
- **Confirm, don't re-send**: `pact msg sent` shows what you sent and whether
  the recipient has read it. If you are unsure a message went out, check
  there — a blind re-send is how a peer's inbox fills with duplicates.
- **Orient with `pact log`**: one chronological feed of who leased what and
  who said what. Read it when you join, and when you need to know whether a
  peer is still moving.
- **Everything is scriptable**: every pact command accepts `--json` for
  machine-readable output; prefer it over parsing human-formatted text.

Run `pact doctor` if anything above seems out of date.
<!-- pact:end -->


<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

