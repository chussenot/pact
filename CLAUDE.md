# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`pact` is a single Rust binary that coordinates multiple coding agents working
on one repository: it injects a coordination protocol into `AGENTS.md`, hands
out advisory file leases, and passes threaded messages between agents through its
own append-only store. **It has no runtime backend** — `bd` is the agents' task
tracker, which pact only ever reads, and only via the committed
`.beads/interactions.jsonl` export.

## Build & test

```bash
mise run check    # fmt-check + clippy (both feature sets) + test + otel + docs — the CI gate
mise run build    # cargo build with every feature
mise run test     # cargo test with no features, then with every feature
mise run install  # cargo install with every feature  (CURRENT build on PATH)
```

Local tasks build **every** feature, from `PACT_ALL_FEATURES` in `mise.toml` —
one list, so no two tasks disagree. None of `ui`, `otel`, `mcp` is a default Cargo
feature: a plain `cargo build` leaves ratatui out and has neither `pact ui` nor
`pact mcp serve`. `pact --version` prints the features compiled in, which is the
fast answer to `unrecognized subcommand`.

`lint` and `test` each run the default build too, because some checks exist only
there (`tests/mcp_absent.rs` is gated `not(feature = "mcp")`). In `test` the
default leg runs **first**: both legs write `target/debug/pact` and the last one
wins, so a featureless binary left behind would make `./target/debug/pact ui`
fail for no visible reason. `mise run otel` uses feature *pairs* on purpose — an
all-features build cannot catch a `#[cfg]` item that needs two features at
once.

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
module. `lease.rs` (advisory locks), `msg.rs` (messaging over
`.pact/messages.jsonl`), `beads.rs` (the only place that reads bd — a private
subprocess runner with two diagnostic callers, `bd --version` and `bd config get
audit.enabled`, plus two read-only parses of `.beads/interactions.jsonl`),
`agents.rs` (who is active),
`events.rs` (the lease-event log behind `pact log`), `agents_md.rs` (the managed
protocol block), `watch.rs` (path subscriptions, and the diff delivered on
release), `audit.rs` (offline analysis of the event log) with `git_history.rs`
(the only place that shells out to `git` for history), `doctor.rs`,
`identity.rs`, `output.rs`, `repo.rs`, and `tui.rs` + `mascot.rs` (the
`pact ui` dashboard).

Invariants worth knowing, because each was broken at least once:

- **Never `println!`.** All output goes through `output::line` / `output::warn`.
  A bare `println!` panics on a closed pipe (`… | head -1`) *after* the side
  effect has landed, so a command that succeeded reports failure — which made
  agents re-send messages they had already sent.
- **Exit codes are API**, documented in `docs/cli.md`: `0` ok, `1` generic, `2`
  lease held by another agent, `3` no `bd` on PATH (**reserved — unreachable from
  every `msg` path since 0.9.0**), `4` not in a git repo, `5` usage error. Raise
  them with `output::exit_with`; `main` maps them via `output::code_for`.
- **A question must not mutate.** Read-only paths use `repo::pact_dir_path`
  (does not create `.pact/`) and `lease::peek` (does not garbage-collect).
  `lease::list` *does* sweep expired locks — that is `lease ls`'s documented job.
- **Never touch the Beads DB directly**, and do not put `bd` back on a command's
  hot path — `beads.rs`'s subprocess runner is private for exactly that reason.
  The one `.beads/` file pact may read is the committed `interactions.jsonl`:
  read-only, best-effort, absent or unparseable means "no beads data" and a PASS.
  Read state is `.pact/read/<agent>.json`, local again after a spell in bd labels;
  a sender can still see acknowledgement because a fleet shares one checkout, and
  `docs/messaging.md` says so rather than inheriting the old guarantee quietly.
- **`agents_md::managed_block()` is the protocol text.** `is_current()` compares
  against it, so editing the text makes every repo report stale until `pact
  init` — that is the freshness check working, not a bug.
- **`tui.rs`: `tab_rects` is shared by rendering and mouse hit-testing** so the
  two cannot drift; widening a tab label without it breaks clicks. The event
  loop's poll timeout is `min(data-refresh remaining, next animation frame)` —
  shortening it naively re-reads and re-parses the whole event and message store
  ~10×/second. It used to spawn a `bd` subprocess that often; the reason for the
  timeout survived the reason for its severity.

## Documentation

`README.md` carries only *why* — the problem, why each primitive is shaped as it
is, the non-goals, provenance. It holds no exit-code table; that contract lives in
`docs/cli.md`. Every *how* lives in `docs/`: `install.md`,
`cli.md` (the command/exit-code contract), `onboarding.md`, `leases.md`,
`messaging.md`, `watch.md`, `architecture.md`, `mcp.md`, `tui.md`,
`telemetry.md`, `development.md`, `testing.md`, `audit.md`,
`fleet-patterns.md`, `mascot-animations.md`.

Every page under `docs/` opens with YAML front matter (`title`, `description`,
`audience`); a new page needs one too. `README.md` has it as well. The three
files that deliberately do NOT: `CHANGELOG.md` is generated by `cog` and would
lose it on the next bump, and `AGENTS.md`/`CLAUDE.md` are read by agents as
instructions rather than rendered as documentation — front matter there is
tokens every agent pays for and nothing renders.

Keep it that way. `scripts/check-docs.sh` (part of `mise run check`) compares
`docs/cli.md`'s `Commands` block against the built binary in both directions,
resolves every relative link and `#anchor`, and requires every `pact doctor`
check name to appear in `docs/tui.md`. After a user-visible change, use the
`docs-curator` agent in `.claude/agents/` rather than editing docs ad hoc — it
holds the placement rules and the reasons behind them.

<!-- pact:begin hash:8546f7af -->
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

