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
   worktree per wave, then checkpoints `.pact/events.jsonl`,
   `.pact/messages.jsonl` and `.beads/interactions.jsonl` in a `chore:` commit for
   the wave.
5. **The orchestrator acts under its own `PACT_AGENT` and `BEADS_ACTOR`**, so its
   merges and checkpoints are not attributed to any worker.

Three files, three reasons, and all of them need committing.

`.pact/events.jsonl` is pact's own coordination history — who held what, and the
only thing it cannot derive.

`.pact/messages.jsonl` is what agents said to each other. It is committed for the
same reason the event log is, and a wave that forgets it produces a history that
can be asked who held a path but never who was warned about it. Both files are
append-only, so `pact init` gives both `merge=union` in `.gitattributes` — without
that, this very pattern conflicts on every wave, in the file agents use to warn
each other.

`.beads/interactions.jsonl` is bd's audit sidecar — and since 0.9.0 it is also the
**only** thing pact reads from bd, so a wave that forgets it leaves
[`--check claim-lease-divergence`](audit.md#--check-claim-lease-divergence) with
nothing to check against. It is off by default; turn it on with `bd config set
audit.enabled true` before the first wave, not after — bd records from that point
and not retroactively, so enabling it afterwards buys the next run, never this one.

**The read cursors are the one thing that stays local.** `.pact/read/<agent>.json`
is per-machine by nature, and committing it would have every clone inherit its
peers' read state. Sharing *who said what* while keeping *who has read it* local is
the line, and it is where `.pact/leases/` and `.pact/waits/` sit too: live runtime
state nobody else should be resolving merges against.

### What it buys

Measured against an unorchestrated build of comparable size: commit provenance
per agent instead of one squashed commit, one bd actor per agent instead of a
single collapsed git identity, and worst-case contention down from eight agents on
one file to three holds on the busiest path.

**One agent per checkout is also what makes read acknowledgement work.** Read
cursors live under `.pact/read/`, which is shared through the same resolution the
leases use, so every agent in a wave can see whether a peer has read a message —
[messaging.md](messaging.md#this-narrows-pact-rnc17-and-says-so-rather-than-inheriting-it-quietly)
explains why that is a property of the topology rather than a guarantee pact makes
everywhere. A fleet split across two machines loses it.

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

## Reserved keys: leasing something that is not a file

Agents invented this before pact had a word for it. In the quern run, three holds were
taken on `.beads` — a directory, not a file — to serialize the agents' own `bd` writes
so two of them would not mutate the store at once. It worked. It was the only non-file
path leased in 57 acquires, and nobody had told them to do it.

A lease is keyed on a path, so it has always been able to stand for something other
than a file. The convention now has a home:

```bash
pact lease acquire .pact/internal/beads-writes --ttl 120 \
  --note "bd close for wave 2"
```

**`.pact/internal/<purpose>` is the reserved namespace.** Anything under it is a
mutex, not a claim on a file, and `pact audit` labels it as one — see below. A
trailing slash works too (`shared-fixtures/`), which is how an agent already spells
"this whole directory" for `pact watch`.

**Short TTLs are correct here, and are not a smell.** The observed idiom was 20 to 180
seconds — long enough for a `bd close`, short enough that a crashed holder blocks
nobody. Letting such a lease lapse instead of releasing it is a legitimate
fire-and-forget: `pact audit` reports expiry-ended holds separately and says so when
their TTL was short, precisely so this pattern does not read as three abandoned
leases.

Why it matters for the record: before this, a mutex hold sat in audit's **most
contended paths** table competing with real source files, and in the quern run `.beads`
ranked second there — above every file it outranked on hold count alone. Mutexes now
sort below files and carry a `[mutex, not a file]` label. They are still reported and
still counted per agent; they are simply not pretending to be file contention.

One honest limit: audit classifies from the path recorded in the log and deliberately
never touches the filesystem, because a log describes a repository state that may no
longer exist and a `stat` would let the same log produce different reports on
different days. A bare directory name like `.beads` carries no marker, so **quern's
own log cannot be reclassified after the fact**. New runs using the reserved prefix
get clean statistics; a legacy bare-directory lease keeps appearing as an ordinary
path.

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
