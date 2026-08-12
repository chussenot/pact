---
title: Fleet patterns
description: The one fleet topology measured twice, the numbers it earned, and where those numbers contradicted the assumption.
audience: orchestrators
---

# Fleet patterns

pact does not require any particular way of running a fleet. This page
describes the one shape that has actually been measured twice, and what the
measurements said — including the two places the numbers contradicted what
people assumed was happening.

Everything here is from two real builds, cross-checked against their committed
`.pact/events.jsonl`, `.beads/interactions.jsonl` and git history. Where a
number is quoted, it came from `pact audit` or `git log`, not from a report.

## The orchestrated-wave pattern

One orchestrator, agents in waves, one git worktree per agent per wave.

1. **The orchestrator creates one worktree per agent**, per wave, on its own
   branch.
2. **Agents lease through the shared namespace** — leases are keyed on
   repo-relative paths, so a lease taken in any worktree contends correctly
   with every other. Nothing extra to configure.
3. **Agents commit in their own worktree**, so `git blame` attributes work to
   the agent that did it.
4. **The orchestrator merges each worktree at wave end**, one merge commit per
   worktree per wave, then checkpoints `.pact/events.jsonl` and
   `.beads/interactions.jsonl` in a `chore:` commit for the wave.
5. **The orchestrator acts under its own `PACT_AGENT` and bd actor**, so its
   merges and checkpoints are not attributed to any worker.

### What it measurably bought

A 20-agent, 5-wave, 63-minute build, against an earlier 8-agent build that did
none of this:

| | earlier build | orchestrated waves |
|---|---|---|
| commit provenance | one 6,167-line commit | 62 commits, 20 of them wave merges |
| bd actor attribution | 1 collapsed git identity | 21 distinct actors |
| worst-case contention | 8 agents on one file | 3 holds on the busiest path |
| lease hygiene | — | 31/31 clean releases, 0 stale holds, longest 10m30s against a 45m TTL |

**Designing the module tree is fleet planning.** The contention spread was not
luck: the file layout was chosen so that concurrent agents would mostly own
different files. That is the technique, and it is worth doing deliberately —
`pact audit`'s "most contended paths" tells you afterwards whether it worked.

### Commit before you release

The one protocol slip the run earned. A fix landed **99 seconds after** its
author had already released the file it changed, and
[`pact audit --check commit-correlation`](audit.md#--check-commit-correlation)
flags it — one of 19 commits touching a leased path outside every hold
recorded for it.

Releasing before committing breaks the binding the log exists to prove. Commit,
then release.

### pact will not see your worktrees unless you run it in them

Every lease in that run was taken from the **main** checkout while the editing
happened in worktrees, because agents inherited the orchestrator's working
directory. It worked — repo-relative keys mean the shared namespace is correct
either way — but the lease/edit binding rested on convention rather than
record.

`pact audit` now says so
([topology](audit.md#where-the-run-actually-happened)). If you want the binding
to be verifiable, run `pact lease` from the worktree the edits happen in.

## What the numbers said about messaging

**This is the part worth reading, because the obvious conclusion was wrong.**

It is tempting to conclude that orchestrated waves make peer messaging
unnecessary — the scheduling pre-resolves contention, so what is there to
negotiate? An early analysis of these two runs concluded exactly that, on the
basis that neither run had sent any messages at all.

It had miscounted. The two runs sent **four** messages between them, and one of
them prevented a runtime panic: an agent that had changed a constant in a file
it owned told the owner of a *different* file which term to update, naming the
exact failure (`write_buffer` overflow once background instances exceeded the
buffer size). The fix landed. Had that message not been sent, the bug ships.

So the accurate claim is narrower than "messaging is rarely needed":

- **Contention messaging is rare in this pattern** — nobody had to negotiate
  for a lease, which is consistent with zero `refused` events across both runs.
  Wave scheduling really does pre-resolve that.
- **Contract messaging is not rare, and is load-bearing.** Changing an API,
  constant, schema or CLI flag that another agent depends on is exactly what
  `pact msg send --to-owner-of <path>` is for, and waves do nothing to remove
  the need.

### But the channel that actually carries it is `pact watch`

Four fleet runs have now produced **zero** voluntary agent-to-agent messages. The
fourth produced **64 watch notifications** in the same window.

That is not a close call, and it is not a story about lazy agents. `pact watch`
delivery rides `lease release` — a command those runs performed 31 times out of 31 —
so it costs an agent nothing at announce time. Messaging asks an agent to remember,
at the moment it is finishing something else, and four runs say it will not.

So under an orchestrated fleet, treat it this way:

| You want | Use |
|---|---|
| to be told when an interface you depend on changes | `pact watch add <path>`, once, at task start |
| an answer, a decision, or something you cannot do yourself | `pact msg send` |

Peer messaging remains load-bearing for exactly the cases watch cannot cover — a
decision you need back, a file you neither own nor watch, a cross-fleet hand-off —
and megablast's single surviving message, the one that kept a `write_buffer` overflow
from shipping, is still the proof. What changed is which one is the *default*.

[`pact audit --check silent-contention`](audit.md#--check-silent-contention) reports
contended paths where neither channel was used, and counts refusals where the asker
had already subscribed.

### Acknowledge what you act on

Of those four messages, **three were never read by the agent they were
addressed to** — including the one that prevented the panic. Two were read by
other agents, which is `--to-owner-of` working as designed (a message about a
path follows the path, so whoever leases it next is often the reader), and one
was never read at all despite being acted on.

The consequence is one-sided: the sender's `pact msg sent` shows those messages
as undelivered permanently, so a sender who checks cannot tell "ignored" from
"handled quietly". `pact audit --export` reports them under
`unacknowledged_messages` ([why not `pact doctor`](messaging.md#and-pact-audit---export-asks-it-for-everybody)).

If you act on a message, run `pact msg read <id>`. It costs one command and it
is the only thing that closes the loop for whoever warned you.

## What this page is not

Not a requirement. pact is a set of primitives — advisory leases, threaded
messages, an append-only log — and unorchestrated peers, long-running agents
and single-agent use are all legitimate. This is the shape that has been
measured, written down so the next fleet starts from evidence instead of from
scratch.

For the primitives themselves see [leases.md](leases.md) and
[messaging.md](messaging.md); for reading a run back afterwards,
[audit.md](audit.md).
