---
title: CLI reference
description: Every command, flag, exit code and --json shape. The contract, checked against the binary in CI.
audience: everyone
---

# CLI reference

Every surface pact exposes, in one place. The reasoning behind each is in the
README and in the topic pages ([leases.md](leases.md),
[messaging.md](messaging.md), [onboarding.md](onboarding.md)); this page is the
contract.

`scripts/check-docs.sh` compares the `Commands` block below against the built
binary's `--help` in both directions, so a subcommand or long flag that exists
in one and not the other fails CI. Do not hand-edit it to match a change you
have not made.

## Who runs what

pact has two audiences and they barely overlap. **Agents** coordinate — claim a
path, hand off a change, subscribe to an interface. **Humans** set the repository
up and read the run back afterwards. Knowing which side a command is on tells you
whether it belongs in a protocol block or in your own terminal.

| | Agents | Humans |
|---|---|---|
| **run these** | `lease`, `msg`, `watch` | `init`, `doctor`, `ui`, `audit`, `completion` |
| **why** | to coordinate while working | to set up, observe, and review |
| **frequency** | many times per task | once per repo, or after a run |

`whoami`, `log` and `agents` are for both: an agent orients with them at task
start, and a human uses the same three to see whether the fleet is moving.

**The dividing line is who bears the consequence.** An agent's `lease acquire` is
a promise it will keep; a human's `audit` is a question about promises already
made. That is also why the MCP server is read-only — see
[mcp.md](mcp.md).

## Commands

```
# ─── agents run these, while they work ───────────────────────────────────────
pact lease acquire <path>... [--ttl <seconds>] [--steal] [--note <text>]
pact lease renew <path>
pact lease release <path>... [--force]
pact lease release --all
pact msg send (--to <agent>... | --to-owner-of <path>...) [--thread <id>] [--subject <text>] [--skip <agent>...] (<body> | --body-file <path|->)
pact msg inbox [--unread-only] [--full] [--include-watch | --watch-only]
pact msg sent
pact msg read <id>
pact watch add <path>
pact watch rm <path>
pact watch ls

# ─── humans run these, around a run ──────────────────────────────────────────
pact init [--print] [--no-commit] [--force]
pact doctor
pact audit [--check <double-win|stale-holds|chain-integrity|commit-correlation|merge-divergence|claim-lease-divergence|retry-storm|silent-contention|topology>] [--expect <worktrees|main|any>] [--since <rfc3339|duration>] [--include-annotated] [--compare <path>] [--export <path>]
pact ui
pact completion <bash|zsh|fish|elvish|powershell>
pact mcp serve

# ─── both ────────────────────────────────────────────────────────────────────
pact whoami
pact agents
pact lease ls [--all]
pact log [-n | --limit <count>]
```

The grouping is a reading aid, not a permission boundary: nothing stops a human
taking a lease or an agent running `pact audit`. It is one fenced block on
purpose — `scripts/check-docs.sh` reads the first fence after this heading and
stops at its close, so splitting it into three would leave two thirds of the CLI
unchecked.

Plus `pact -V` (bare version) and `pact --version` (version plus build stamp —
see [install.md](install.md#which-binary-am-i-running)).

Two of those exist only in a build that asked for them: `pact ui` needs the `ui`
feature and `pact mcp serve` needs `mcp`. In a build without one, the subcommand
is absent from `--help` entirely and invoking it is a usage error (exit 5, not
2 — see below). `pact --version` lists the features compiled in, which is the
fast answer to `unrecognized subcommand`. `pact mcp serve` is documented in
[mcp.md](mcp.md); it is read-only and speaks MCP on stdio.

Every subcommand accepts a global `--agent <name>` (or `PACT_AGENT` env var)
and `--json` flag. `--all` on `release` is mutually exclusive with both
`<path>` and `--force`; `--body-file` is mutually exclusive with the positional
body. clap rejects those combinations rather than silently ignoring one.

`init --force` writes through a live lease on a file `init` would rewrite;
without it `init` exits 2 and writes nothing at all
([why](onboarding.md#init-refuses-to-write-through-a-live-lease)). It is the
same explicit-override shape as `acquire --steal` and `release --force`, and
like both it is unrelated to `--no-commit`, which only skips the commit.

Batching doesn't change the shape a one-path script already parses: a single-path
`lease acquire --json` still emits the lease *object* (several paths emit an
array), and a single `--to` still prints `sent <id> to <who> (thread <id>)`.
`lease release --json` now emits an object — `{"path": …, "displaced": …}` — so a
scripted caller can see whose claim a `--force` destroyed.

A `msg send` to several recipients that fails partway through fails as a
structured error under `--json` — `{"already_sent": […], "failed_at": …,
"reason": …}`, on **stdout** — so a retry can pass `--skip <agent>` (repeatable)
for each already-sent name instead of duplicating delivery to them. See
[messaging.md](messaging.md#replaying-a-fan-out-that-failed-partway---skip).

**Every other `--json` failure gets `{"error": …, "exit_code": …}`, also on
stdout.** Before this, a failure printed only plain text to stderr regardless
of `--json` — so the single most routine non-zero outcome two agents
contending on a file will ever produce (a lease conflict, exit 2) gave a
`--json` caller an empty stdout to parse. Without `--json`, nothing changes:
the human-readable text is unchanged and still goes to stderr.

`pact completion <shell>` prints a completion script on stdout. It is
**generated from the same command tree clap parses with**, so it cannot drift
out of step with the binary — which is why it is a command rather than five
scripts checked into the repo. Where each shell wants it:

```bash
pact completion bash > /etc/bash_completion.d/pact          # or ~/.local/share/bash-completion/completions/pact
pact completion zsh  > "${fpath[1]}/_pact"
pact completion fish > ~/.config/fish/completions/pact.fish
```

It needs no repository: unlike every other command it reads no state, and a
shell profile is sourced from `$HOME` rather than from a checkout. Regenerate
it after upgrading pact — a script written by an older binary completes that
binary's commands, which is the one way this can still go stale.

`audit --export <path>` writes one combined JSON snapshot — the summary,
every named check and `pact doctor`'s checks — to a file, orthogonal to
whatever `--check`/no-`--check` and `--json` decide for stdout: pass it
alongside either. Under `--json` its own confirmation is skipped rather than
printed as a second top-level object, so stdout stays the one parseable value
every other command promises; in human mode it prints the path it wrote. See
[audit.md](audit.md#--export).

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | generic error — and `pact audit --check …` found something (a finding is a result, not a fault) |
| 2 | lease conflict — held by another agent, or you don't hold the one you're releasing, or `init` found one on a file it rewrites |
| 3 | Beads backend unavailable — no `bd` on `PATH`, a `br`-only workspace ([no longer supported](install.md#br-beads-rust-is-no-longer-supported)), or a call killed for running past `PACT_BEADS_TIMEOUT_SECS` |
| 4 | not in a git repository |
| 5 | usage error — unknown subcommand, bad or missing flag value |

**3 covers every way the backend can be unusable.** A `bd` that never
exits — wedged on a credential prompt, a backend write lock, an internal bug —
used to hang the pact command that called it, and everything built on it,
forever. That wait is now bounded: past `PACT_BEADS_TIMEOUT_SECS` (default 30
seconds — `DEFAULT_BEADS_TIMEOUT_SECS` in `src/beads.rs`) the child is killed and
the error names the variable to raise. A subprocess that never comes back is the
same class of problem as one that was never there, so it reuses 3 rather than
adding a code every caller would have to learn.

**5 exists so that 2 means only one thing.** clap emits 2 for any usage error,
which collided with "lease held by another agent" — and a wrapper branching on 2
read a typo as a lease conflict and went off to negotiate with a peer that does
not exist. Two agents hit that in one fleet run: an unrecognized subcommand, and
a `--thread` left valueless by shell word-splitting. The flag case is the likelier
one in a script, because a flag value is exactly what gets interpolated from a
variable. `pact --help` and `pact -V` still exit 0; bare `pact` is a usage error
and exits 5, so a script whose variable expanded to nothing cannot read it as
success.

`pact doctor` exits 1 when a check **fails** (`✗`). A check can also **warn**
(`!`) — it passed, but you should know: a Beads CLI outside its tested version range,
or protocol files a clone won't see. Warnings never change the exit code, and
`--json` carries them as `"warn": true` alongside `"ok": true`, so a script can
tell the two apart. `pact whoami` is the one command that always exits 0: a
missing identity, a missing `bd`, or an unreadable repo root are reported as
`!` problems, not raised.

**A closed pipe is not one of these codes.** `pact … | head -1` used to panic
mid-write and exit 101, which an agent reading only the status could not tell
from "the send failed" — so it retried, and the fleet got duplicate messages.
pact now drops the unwritten bytes silently and keeps whatever status its actual
work earned, normally 0. That is deliberate rather than the conventional
SIGPIPE-emulating 141: the side effect (the bead created, the lock file written)
has already landed by the time anything is printed, and losing the tail of a
report whose reader walked away is cheaper than making a completed action look
failed.

