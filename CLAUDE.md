# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`pact` is a single Rust binary that coordinates multiple coding agents working
on one repository: it injects a coordination protocol into `AGENTS.md`, hands
out advisory file leases, and passes threaded messages between agents by
shelling out to the Beads CLI (`bd`).

## Build & test

```bash
mise run check    # fmt-check + clippy (both feature sets) + test + otel + docs — the CI gate
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
## pact coordination protocol

Claude Code loads this file, not `AGENTS.md`, so the protocol is imported
here instead of copied — one source of truth, in the file the other agents
already read. Run `pact init` to refresh it.

@AGENTS.md
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

