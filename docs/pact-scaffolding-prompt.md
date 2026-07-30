# Scaffolding prompt — `pact` (agent coordination CLI)

> Copy everything below into Claude Code (or your coding agent) at the root of an empty repo.

---

You are scaffolding a new Rust project. Read this entire brief before writing any code, then create the full project structure, implement it, and make all tests pass.

## Project

**Name:** `pact` (binary name: `pact`)
**Purpose:** A single, dependency-light CLI that coordinates multiple coding agents working on the same repository. It has exactly three responsibilities:

1. **Onboarding** — idempotently inject/update a managed section into `AGENTS.md` that teaches agents the coordination protocol.
2. **Messaging** — thin wrapper over the Beads CLI (`br`, fallback `bd`) message issue type, so agents send/read threaded messages without knowing Beads flags.
3. **Leases** — advisory file leases via atomic lock files, with TTL and steal semantics.

**Explicit non-goals (do NOT implement):** no daemon, no background process, no MCP server, no direct read/write of the Beads database or JSONL (always shell out to the Beads CLI), no mandatory locking, no network I/O, no config file in v1.

## Architecture principles

- Rust 2021 edition, stable toolchain. Single binary, no runtime dependencies beyond the optional `br`/`bd` on PATH.
- Crates: `clap` (derive) for CLI, `serde`/`serde_json`, `anyhow` for application errors + `thiserror` for library errors, `chrono` for timestamps, `tempfile` (dev-dependency) for tests. Nothing else without strong justification — justify any addition in a code comment.
- Layout: `src/main.rs` (CLI parsing + dispatch only), `src/agents_md.rs`, `src/lease.rs`, `src/msg.rs`, `src/beads.rs` (subprocess adapter), `src/identity.rs`, `src/output.rs` (human vs `--json` rendering).
- Every subcommand supports `--json` for machine-readable output (agents are the primary consumers). Human output is secondary.
- Exit codes are part of the API: `0` success, `1` generic error, `2` lease held by another agent, `3` Beads CLI not found, `4` not in a git repo. Document them in `--help` and README.
- All filesystem state lives under `.pact/` at the repo root (found by walking up to `.git`). Nothing outside the repo.

## Agent identity

- Resolution order: `--agent <name>` flag → `PACT_AGENT` env var → error with a clear message. Never guess or generate an identity.
- Validate: `[a-z0-9][a-z0-9-]{1,31}`.

## Subcommand spec

### `pact init`
- Creates `.pact/leases/` and appends a managed block to `AGENTS.md` (creates the file if absent).
- The managed block is delimited by `<!-- pact:begin -->` / `<!-- pact:end -->` markers. If the block exists, replace its content in place (idempotent — running `init` twice produces zero diff). Never touch anything outside the markers.
- Block content: the coordination protocol for agents, ~20 lines: identity via `PACT_AGENT`, check inbox at task start (`pact msg inbox`), acquire a lease before editing shared files (`pact lease acquire <path>`), release when done, send a message on interface changes, all commands support `--json`.
- `--print` outputs the block to stdout without writing.
- Add `.pact/leases/` to `.gitignore` (managed the same idempotent way, single line, only if missing). Leases are local runtime state, never committed.

### `pact lease acquire <path> [--ttl <seconds>] [--steal] [--note <text>]`
- Default TTL: 900s. `<path>` is normalized relative to the repo root.
- Lock file: `.pact/leases/<encoded>.lock` where `<encoded>` replaces `/` with `__` (document the collision caveat with real `__` in paths; acceptable in v1).
- Creation MUST be atomic: open with `create_new(true)` (O_EXCL). Payload: `{"agent","path","acquired_at","ttl_secs","note"}` as JSON.
- If the lock exists: if expired (`acquired_at + ttl < now`), atomically replace it (write to `.pact/leases/tmp-<uuid>`, then rename) and report `stolen: true`. If held by the same agent, refresh `acquired_at` (re-entrant). Otherwise exit 2, printing holder, age, remaining TTL.
- `--steal` forces takeover of a non-expired lease (prints a warning; the point is advisory coordination, not enforcement).

### `pact lease release <path>`
- Only the holder may release; releasing a lease you don't hold is exit 2 (`--force` overrides). Releasing a non-existent lease is success (idempotent).

### `pact lease ls [--all]`
- Lists active leases (holder, path, age, remaining TTL). Expired leases are hidden by default and garbage-collected on any `lease` invocation.

### `pact msg send --to <agent> [--thread <id>] [--subject <text>] <body>`
- Shells out to the Beads CLI: locate `br` then `bd` on PATH (cache the choice per invocation, not on disk). Construct the equivalent of a message-type issue with threading. **Discover the exact flags at implementation time by running `br --help` / `br create --help` in the sandbox; do not invent flags.** If the CLI is absent, exit 3 with an actionable install hint.
- `--thread` defaults to a new thread; print the thread id so agents can reply.

### `pact msg inbox [--unread-only]` and `pact msg read <id>`
- Query messages addressed to the current agent via the Beads CLI with `--json`, parse, render. Reading marks as read only if the underlying Beads lifecycle supports it — otherwise track read state in `.pact/read.json` (local, gitignored) and say so in a comment.

### `pact doctor`
- Checks: in a git repo, `.pact/` present, AGENTS.md block present and current, Beads CLI found (which one, version), stale locks count. Exit 0 only if all pass. This is what CI and humans run first.

## Error handling & robustness

- Every subprocess call captures stderr and surfaces it on failure (`anyhow::Context` everywhere).
- Clock skew tolerance: treat leases as expired only when `now > acquired_at + ttl + 30s` grace.
- All lock writes go through the write-temp-then-rename pattern; never leave a partial lock.
- No panics on user input; `unwrap` allowed only where invariants are proven, with a comment.

## Tests (required, must pass)

- Unit: path encoding round-trip; identity validation; managed-block idempotency (apply twice → identical file); expiry math including grace.
- Integration (`tests/`, using `tempfile` + `git init`): acquire → conflict from second agent (exit 2) → expiry → steal; re-entrant refresh; release idempotency; `init` on a repo with an existing AGENTS.md containing unrelated content (must be preserved byte-for-byte outside markers).
- For `msg`: mock the Beads CLI with a shell-script stub on PATH recording its argv; assert the constructed command. Do not require a real `br` in CI.

## Deliverables

1. Full crate compiling with zero `cargo clippy --all-targets -- -D warnings` issues, formatted with `cargo fmt`.
2. `README.md`: purpose, install, the protocol (same content as the AGENTS.md block), exit codes table, FAQ entry "why advisory and not mandatory locking".
3. `CHANGELOG.md` with `0.1.0`.
4. A GitHub Actions workflow: fmt check, clippy, test on ubuntu-latest + macos-latest.

## Working method

Work in this order: (1) crate skeleton + CLI surface with `todo!()` bodies and passing compilation; (2) `lease` module + its tests; (3) `agents_md` module + tests; (4) `beads` adapter + `msg` + stub tests; (5) `doctor`, README, CI. Commit after each step with a conventional-commit message. If a Beads CLI flag you need doesn't exist, stop and report the gap instead of guessing.
