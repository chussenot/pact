# pact

A single, dependency-light CLI that coordinates multiple coding agents working
on the same repository. It has exactly three responsibilities:

1. **Onboarding** — idempotently inject/update a managed section in
   `AGENTS.md` that teaches agents the coordination protocol below.
2. **Messaging** — a thin wrapper over the [Beads](https://github.com/gastownhall/beads)
   CLI (`bd`) so agents can send/read threaded messages without knowing Beads
   flags.
3. **Leases** — advisory file leases via atomic lock files, with TTL and steal
   semantics.

It does not run a daemon or MCP server, does not touch the Beads database or
JSONL directly (it always shells out to `bd`), does not enforce locking, and
has no config file. v0.1.0 targets `bd` (beads) only; `br` (beads-rust)
compatibility is a deliberate later phase.

## Install

```bash
cargo build --release
cp target/release/pact /usr/local/bin/  # or anywhere on your PATH
```

Requires `bd` (beads) on `PATH` for the `msg` subcommands; `init`, `lease`,
and `doctor` (partially) work without it.

## The protocol

Running `pact init` writes this block into `AGENTS.md` (between
`<!-- pact:begin -->` / `<!-- pact:end -->` markers, so re-running it is a
no-op once nothing has changed):

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

## Commands

```
pact init [--print]
pact lease acquire <path> [--ttl <seconds>] [--steal] [--note <text>]
pact lease release <path> [--force]
pact lease ls [--all]
pact msg send --to <agent> [--thread <id>] [--subject <text>] <body>
pact msg inbox [--unread-only]
pact msg read <id>
pact doctor
```

Every subcommand accepts a global `--agent <name>` (or `PACT_AGENT` env var)
and `--json` flag.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | generic error |
| 2 | lease held by another agent (or you don't hold the lease you're releasing) |
| 3 | Beads CLI (`bd`) not found on `PATH` |
| 4 | not in a git repository |

## FAQ

**Why advisory locking instead of mandatory?** Coding agents already fail in
ways a mandatory lock can't prevent — crashing mid-edit, ignoring the tool
entirely, or editing through a channel pact doesn't see. A lease that can be
stolen (`--steal`) or expires on its own (TTL + a clock-skew grace period)
degrades to "nothing happened" instead of a stuck repository nobody can
unblock. Advisory coordination costs agents a moment of politeness; mandatory
locking costs you a deadlock at 2am with no daemon to ask why.

## Development

```bash
cargo build
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
```

State lives under `.pact/` at the repo root (found by walking up to `.git`):
`.pact/leases/*.lock` (gitignored by `pact init`) and `.pact/read.json`
(message read-state, also gitignored, since Beads has no read/unread
lifecycle for message issues).
