---
title: Fleet patterns
description: How to run a fleet on pact — the orchestrated-wave topology, what it buys, and the two rules it exists to make possible.
audience: orchestrators
---

# Fleet patterns

pact does not require any particular way of running a fleet. This page describes
the one shape that has been measured, and what you have to do to get its
benefits.

Not a requirement: unorchestrated peers, long-running agents and single-agent use
are all legitimate. This is the shape with evidence behind it, so the next fleet
starts from something better than a guess.

For the numbers — which runs, how many agents, what each measured — see
[studies/field-runs.md](studies/field-runs.md).

## The orchestrated-wave pattern

One orchestrator, agents in waves, one git worktree per agent per wave.

1. **The orchestrator creates one worktree per agent**, per wave, on its own
   branch.
2. **Agents lease through the shared namespace.** Leases are keyed on
   repo-relative paths, so a lease taken in any worktree contends correctly with
   every other. Nothing extra to configure.
3. **Agents commit in their own worktree**, so `git blame` attributes work to the
   agent that did it.
4. **The orchestrator merges each worktree at wave end**, one merge commit per
   worktree per wave, then checkpoints `.pact/events.jsonl` and
   `.beads/interactions.jsonl` in a `chore:` commit for the wave.
5. **The orchestrator acts under its own `PACT_AGENT` and bd actor**, so its
   merges and checkpoints are not attributed to any worker.

### What it buys

Measured against an unorchestrated build of comparable size: commit provenance
per agent instead of one squashed commit, one bd actor per agent instead of a
single collapsed git identity, and worst-case contention down from eight agents on
one file to three holds on the busiest path.

**Designing the module tree is fleet planning.** That contention spread was not
luck — the file layout was chosen so concurrent agents would mostly own different
files. It is worth doing deliberately, and `pact audit`'s most-contended-paths
output tells you afterwards whether it worked.

## The two rules that make the record trustworthy

Both were learned by getting them wrong, and neither is enforced.

### Commit before you release

A lease released while the work is still uncommitted breaks the one binding the
log exists to prove. In one run a fix landed 99 seconds after its author had
already let the file go, and
[`--check commit-correlation`](audit.md#--check-commit-correlation) reports it as a
commit no hold covered.

Commit, then release.

### Run pact from the worktree the edits happen in

Agents inherit the orchestrator's working directory unless told otherwise, so it
is easy to end up editing in a worktree while every lease is taken from the main
checkout. It *works* — repo-relative keys make the namespace correct either way —
but the lease/edit binding then rests on convention rather than record.

`invoked_from` on every event is what makes it checkable, and
[`--check topology --expect worktrees`](audit.md#--check-topology---expect-worktreesmainany)
turns it into a gate.

## Which channel carries what

Two mechanisms overlap, and picking wrongly is the most common source of noise in
a fleet:

| You want | Use |
|---|---|
| to be told when an interface you depend on changes | `pact watch add <path>`, once, at task start |
| an answer, a decision, or something you cannot do yourself | `pact msg send` |

**Prefer `watch` for announcements.** Its delivery rides `lease release`, so it
costs an agent nothing at the moment it is finishing something else — which is
exactly when a voluntary announcement gets skipped. Across the runs where watch
was live it delivered 87 and 64 diffs without anyone remembering to.

**Messaging stays load-bearing for what needs an answer.** One message in one run
is the only reason a `write_buffer` overflow did not ship, and no volume rule can
predict which message that will be. Reserve it for things you need something back
on, not for progress.

Two commands read the result back:
[`--check silent-contention`](audit.md#--check-silent-contention) reports contended
paths where neither channel was used, and `pact audit --export` lists messages
their own recipient never marked read.

### Acknowledge what you act on

`pact msg read <id>` is the only thing that tells a sender their warning landed.
Act on a message without it and their `pact msg sent` says "undelivered" forever,
which is indistinguishable from being ignored — and in one run three of four
messages were never read by the agent they were addressed to, including the one
that prevented the panic.

It costs one command.

## Related

For the primitives themselves, [leases.md](leases.md) and
[messaging.md](messaging.md); for reading a run back afterwards,
[audit.md](audit.md); for the evidence behind everything on this page,
[studies/field-runs.md](studies/field-runs.md).
