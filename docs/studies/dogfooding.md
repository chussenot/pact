---
title: Dogfooding
description: What building pact with pact taught, including the two protocol reversals and the findings that only appear when the tool coordinates its own construction.
audience: contributors
---

# Dogfooding

pact coordinates the fleets that build pact. Almost nothing in it was designed on
a whiteboard: a fleet was pointed at this repository and every agent was required
to report, with quoted commands and exit codes, what pact had actually done to
it.

That method is the point, and it is reusable: **if you run agents against your
own repo, ask each one what the tooling did to it, and require a quoted command
as evidence.** An opinion about a tool's ergonomics is worth very little; an exit
code is worth acting on.

The [field runs](field-runs.md) are pact used to build four other things. This
page is pact used to build pact, which surfaces a different class of finding —
the ones you only hit when the tool is under the same load as the work.

## The four findings that shaped the CLI

Each of these is a quoted command from an agent's report, not a preference.

- **`pact lease ls` used to lead with remaining TTL.** A lease 80 seconds old
  printed `3520s`, an operator read that as "long-held", and force-released a live
  agent's claim. Hence age first, an explicit state, and the holder's note — "what
  is this agent doing" is the question you have right before you reach for
  `--force`.
- **`pact msg inbox` used to print every body in full and never showed a sender.**
  Seven messages were roughly 9KB of an agent's context window, paid on every
  inbox check the protocol asks for, and `pact msg read` was pointless because
  nothing was left to read. Hence one line each, and a sender column.
- **`pact msg inbox | head -1` panicked and exited 101.** The message had already
  been sent; the agent read the status, concluded failure, and sent again. Hence
  the rule that no pact command may `println!` — a bare macro panics on a closed
  pipe *after* the side effect has landed, so a command that succeeded reports
  failure. It is the only invariant in this repo whose violation manufactures
  duplicate work.
- **51 of 59 messages in one run were never read**, because they were addressed to
  agents that had already exited. Hence delivery that follows the file rather than
  the name.

## The protocol block was tuned twice, in opposite directions, and overshot both

This is the most useful thing dogfooding produced, because it is a finding about
*documentation* rather than about code, and it generalises past pact.

**Unrestrained, agents spammed.** Before the block said anything to hold messaging
back, pact's own fleet runs produced **223 message beads**. One run alone sent 85
messages of which **41 were status pings** — "X starting", "X done" — addressed to
a recipient who reads a dashboard, not a mailbox. A real
`BLOCKER: pact init deletes the protocol` sat unread for **38 minutes** inside
that noise.

**Restrained, they went silent.** So the block was rewritten: the lease note *is*
the announcement, and messages are reserved for what needs something back. Across
the three runs after that change, **28 agents sent 4 messages between them** —
pact's own fleet 0, arkanoid 3, megablast 1. And the collapse took the
load-bearing messages with it: one of those four was the only reason a
`write_buffer` overflow did not ship.

The conclusion is not that agents are lazy:

> **A voluntary step off the critical path is bimodal under prose. Spam or
> silence, with no reachable middle** — exhortation cannot dial it, because the
> behaviour is not on the path the agent is being graded on either way.

Every mechanism decision since has followed from that sentence. `pact watch` asks
for nothing at announce time and delivers as a side effect of a command agents
already run 31 times out of 31. The refusal message now names the number to back
off by instead of saying "retry later". Where prose was unavoidable, it carries a
measurement, because three runs in a row showed vague guidance losing to whatever
the agent harness did by default.

## Findings that only appear when the tool builds itself

- **`pact init` rewrote files that were under a live lease.** A tool that tells
  every agent to lease what it writes, and then writes through those leases
  itself, is teaching that the protocol is optional. `init` now refuses.
- **A stale binary on `PATH` silently regressed `AGENTS.md` and `.gitignore`.** An
  agent ran `pact init` from an old build, which rewrote both with its old rules,
  and nothing said so. Hence the rule to `mise run install` after changing
  anything, and `pact --version` printing the compiled features — the fast answer
  to `unrecognized subcommand`.
- **The managed block is versioned by content hash.** Editing the protocol text
  makes every repository report stale until `pact init`, which is the freshness
  check working rather than a bug. It exists because a run that straddles a block
  change is otherwise indistinguishable from one that did not.
- **The CLI had no end-to-end coverage, and a batch of bugs shipped precisely
  there.** Hence `tests/cli.rs` driving the real binary through
  `env!("CARGO_BIN_EXE_pact")`, and the convention that CLI behaviour is tested
  against the binary rather than the library.
- **`.pact/events.jsonl` is the one thing under `.pact/` that is committed**, and
  the only thing pact stores that it cannot derive from anything else. Left
  uncommitted, every clone starts with no coordination history and nobody can ask
  afterwards who held what.

## The gate, and one hazard it has

`mise run check` is the CI gate: fmt, clippy across both feature sets, shellcheck,
tests on the default *and* all-features builds, the `otel` feature pairs, and
`scripts/check-docs.sh`. Each leg exists because something got through without it
— the docs checker was written after a review found 13 defects accumulated over 14
code commits, the worst of which told readers to build with a command that
produces a binary with no `pact ui` in it at all.

One hazard is worth knowing because it costs a full re-run to diagnose: **the test
task's two legs both write `target/debug/pact`**, and any concurrent `cargo`
invocation joins that race. The symptom is all seven `tests/mcp.rs` tests failing
at once with exit code 5 — clap's `unrecognized subcommand`, because the binary on
that path momentarily has no `mcp` feature. It reads exactly like a real
regression and is not. Run the gate alone and touch nothing else until it exits.

## What dogfooding cannot tell you

It is one repository, one language, one team of one. The findings above are about
pact's ergonomics under agent use, which generalises; they say nothing about how
pact behaves in a repository whose module tree was not designed by somebody
reading `pact audit`'s contention output. That is what the
[field runs](field-runs.md) are for, and it is why they are built as *other*
things rather than as more pact.
