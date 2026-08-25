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
pact lease acquire <path>... [--ttl <duration>] [--steal] [--note <text>] [--bead <id>] [--wait <duration>]
pact lease renew <path>
pact lease release <path>... [--force]
pact lease release --all
pact lease sweep [<path>...] [--suspect]
pact merge <branch> [--verify <command>] [--ttl <duration>] [--allow-dirty]
pact msg send (--to <agent>... | --to-owner-of <path>...) [--thread <id>] [--subject <text>] [--skip <agent>...] (<body> | --body-file <path|->)
pact msg inbox [--unread-only] [--full] [--include-watch | --watch-only]
pact msg sent
pact msg read <id>
pact msg thread <key> [--brief]
pact watch add <path>
pact watch rm <path>
pact watch ls

# ─── humans run these, around a run ──────────────────────────────────────────
pact context set <key> <value>
pact plan lint <manifest>
pact handoff <bead> --confidence <high|medium|low> --findings <text|@file>
pact init [--print] [--no-commit] [--force]
pact doctor [--fix]
pact audit [--check <double-win|stale-holds|chain-integrity|commit-correlation|merge-divergence|claim-lease-divergence|retry-storm|silent-contention|topology|gate-order>] [--expect <worktrees|main|any>] [--since <rfc3339|duration>] [--include-annotated] [--compare <path>] [--export <path>] [--allow-main <agent>...] [--strict]
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
and `--json` flag.

**Attribution comes from the environment and has no flags.** `PACT_MODEL`,
`PACT_HARNESS`, `PACT_HARNESS_SESSION` and `PACT_HARNESS_SUBAGENT` are stamped on
every event and every message a process writes; a spawner sets them once beside
`PACT_AGENT` rather than every command repeating them. Each is optional and each
is absent — never `"unknown"` — when unset. `pact doctor`'s `attribution` check
prints what this process resolved. See
[harness-detection.md](harness-detection.md) for what pact reads and what it
refuses to guess, and [fleet-patterns.md](fleet-patterns.md) for the spawner side.

`lease ls` grows a `VIA` column when at least one live lease carries a harness or
a declared model, on the same rule as its existing `WHERE` column: a fleet that
declares nothing sees byte-identical output to before.

`acquire --wait <duration>` blocks until a held path is free rather than exiting 2
immediately, and still exits 2 with the same refusal if the budget runs out. It
exists because a subagent cannot wait any other way — its process is its turn
loop, so ending the turn to wait for a `pact watch` notification is the same as
exiting. See [leases.md](leases.md#waiting-for-a-held-path---wait). `--all` on `release` is mutually exclusive with both
`<path>` and `--force`; `--body-file` is mutually exclusive with the positional
body. clap rejects those combinations rather than silently ignoring one.

**`<duration>` is one grammar everywhere it appears** — `<n><unit>` with unit in
`s`, `m`, `h`, `d`, `w`, so `45m`, `24h`, `7d`, `2w` all mean what they read as
to both `lease acquire --ttl` and `audit --since`. A bare number is seconds, for
the scripts that already pass one. `--ttl` warns on a bare value under 120 and
holds anyway ([why](leases.md#--ttl-takes-a-duration-and-a-small-bare-number-warns));
a malformed one is a usage error, exit 5.

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

**Every `--json` failure gets `{"error": …, "exit_code": …}` on stdout.** Before
this, a failure printed only plain text to stderr regardless of `--json` — so the
single most routine non-zero outcome two agents contending on a file will ever
produce (a lease conflict, exit 2) gave a `--json` caller an empty stdout to
parse. Without `--json`, nothing changes: the human-readable text is unchanged and
still goes to stderr.

**One shape was removed rather than kept for compatibility.** A `msg send` to
several recipients used to be N writes, so it could fail partway through and
reported `{"already_sent": […], "failed_at": …, "reason": …}` so a retry could
`--skip` whoever already had it. A send is now a single append that cannot
partially fail, so that shape is gone and a failing send returns the ordinary
`{"error": …, "exit_code": …}`. `--skip <agent>` (repeatable) survives, meaning
simply "leave this recipient out of this send" —
[messaging.md](messaging.md#--skip-leaving-a-recipient-out).

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
binary's commands, which is the one way this can still go stale. `mise run
install` does that for you, rewriting whichever of those three files already
exists; a shell that has already loaded one picks the new one up on its next
start.

**`pact context set <key> <value>` records a constraint the run operates under**,
as a `context` row in `.pact/events.jsonl`, chain-hashed like every other event.
It is not metadata kept alongside the log; it is in the log, because the whole
point is that it still be there when someone audits the run months later.

Keys are free-form — no whitespace and no `=`, so a row always renders
unambiguously as `key=value`; the value is free text and may contain both.
Setting a key twice keeps both rows and the later value wins, because a run that
revised its policy did revise it. Context rows are **not** counted as behaviour
by `pact audit`, for the same reason annotations are not: a row describing the
run is not a thing the fleet did.

Two checks read it. `--check commit-correlation` reports
`commit policy: none — correlation not evaluated` under `commit-policy=none` or
`orchestrator-only`, instead of reporting holds-without-commits as findings when
no agent was permitted to commit. `--check topology` takes its expectation from
`topology-expectation` when `--expect` is absent, so a run is audited against
what it declared rather than what someone remembers; an explicit `--expect` still
wins. The starter vocabulary is in
[fleet-patterns.md](fleet-patterns.md#recording-the-constraints-a-run-ran-under),
and *why* this exists is
[audit.md](audit.md#what-the-log-cannot-tell-you).

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
| 3 | **RETIRED in 0.9.0.** Formerly "Beads backend unavailable". No command raises it — never reuse it for a new meaning, because wrappers in the field still branch on it |
| 4 | not in a git repository |
| 5 | usage error — unknown subcommand, bad or missing flag value |

**3 is retired, not repurposed.** Until 0.9.0 every
`pact msg` command located and ran a Beads CLI first, so exit 3 was the routine
answer to no `bd` on `PATH`, a `br`-only workspace
([no longer supported](install.md#br-beads-rust-is-no-longer-supported)), or a
call killed for running past `PACT_BEADS_TIMEOUT_SECS`. Messages now live in
`.pact/messages.jsonl`, so `msg send`/`inbox`/`read`/`sent`, watch delivery and
`lease acquire`'s check for mail about a path all work with **no `bd` installed at
all** ([messaging.md](messaging.md#why-this-is-not-in-the-issue-tracker-any-more)).

The commands that still look for `bd` do not raise it either: `pact doctor` reports
it as a check, `pact whoami` as one of the problems it always exits 0 despite, and
`pact ui` as a line in its status pane. The single site that can produce it,
`BeadsCli::locate`, has no caller left that propagates it — asserted by a test that
drives the whole command surface with `bd` hidden from `PATH` and requires that
nothing exits 3.

It stays retired rather than recycled because a 0.8.x caller still tests for it, and
a re-used code is how such a wrapper silently starts doing the wrong thing.

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
(`!`) — it passed, but you should know: protocol files a clone won't see, two
Beads stores in one `.beads/`, or bd's audit sidecar switched off so
`--check claim-lease-divergence` has nothing to read. Warnings never change the exit code, and
`--json` carries them as `"warn": true` alongside `"ok": true`, so a script can
tell the two apart. `pact whoami` is the one command that always exits 0: a
missing identity, a missing `bd`, or an unreadable repo root are reported as
`!` problems, not raised.

**`pact doctor --fix` repairs what pact owns, and nothing else.** Bare `doctor`
is a question and stays one — the flag is the explicit opt-out from *a question
must not mutate*. What it repairs is exactly what `pact init` writes, by calling
init's own writers rather than a second copy of them: the managed block, the
files that must point at it (`CLAUDE.md`, `GEMINI.md` and friends), the ignore
rules that decide whether the event log and message store reach a clone, and
pact's own `staging-*` debris under `.pact/leases/`.

It refuses the rest **by name, with the reason**, rather than passing over them
in silence — an operator looking at a check that is still red has to know whether
pact tried and failed or never tried:

| Refused | Why |
|---|---|
| `corrupt leases` | a lock pact cannot read is evidence; only a human can judge whether clearing it is safe |
| `no duplicated instruction blocks` | the repeated heading is in another tool's block, and pact editing a section it does not own is the bug |
| `write-set symlinks` | a managed file symlinked outside the repository — writing it is precisely what must not happen |
| `one Beads store` | pact never writes to `.beads/` |
| `stale wait markers` | ordinary fleet behaviour rather than damage, and never part of health |

Note the fixed set is not derived from `✗` versus `!`: the ignore rules only
*warn* and are among the most worth repairing, while `corrupt leases` *fails* and
must never be touched.

It **never commits** — that is `init`'s job — and it obeys the same refusal
`init` does, exiting 2 without writing anything when one of its targets is under
another agent's lease. There is no `--force`, because `pact init --force` already
is that command.

Exit codes are doctor's: 0 once healthy, 1 with failures remaining, 2 refused.

**A closed pipe is not one of these codes.** `pact … | head -1` used to panic
mid-write and exit 101, which an agent reading only the status could not tell
from "the send failed" — so it retried, and the fleet got duplicate messages.
pact now drops the unwritten bytes silently and keeps whatever status its actual
work earned, normally 0. That is deliberate rather than the conventional
SIGPIPE-emulating 141: the side effect (the bead created, the lock file written)
has already landed by the time anything is printed, and losing the tail of a
report whose reader walked away is cheaper than making a completed action look
failed.

