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

## Recording the constraints a run ran under

**Do this before you spawn the first agent.** A fleet's log records what it did,
never what it was told, and the two are indistinguishable afterwards — see
[audit.md, "what the log cannot tell you"](audit.md#what-the-log-cannot-tell-you)
for the case where an audit read a top-down instruction as an emergent
mechanism and built a feature recommendation on it.

```bash
pact context set commit-policy none
pact context set scheduler pre-serialized
pact context set topology-expectation worktrees
```

Keys are free-form. This is the starter vocabulary, and it covers the three
things an auditor most often has to guess:

| Key | Values | What it stops a reader inferring |
|---|---|---|
| `commit-policy` | `none`, `per-task`, `orchestrator-only` | that holds without commits are agents failing to commit, when they were told not to. `none` and `orchestrator-only` make `--check commit-correlation` report `correlation not evaluated` rather than findings |
| `scheduler` | `waves`, `free-run`, `pre-serialized` | that repeated holds on one file were contention pact arbitrated, when the harness had already ordered them |
| `topology-expectation` | `worktrees`, `main`, `any` | nothing — but `--check topology` reads it when `--expect` is absent, so the run is audited against what it declared instead of what someone remembers |

Anything else is an operator note, and worth writing when it is the kind of thing
you would otherwise put in a message to yourself:

```bash
pact context set note "agents told to leave the tree dirty for human review"
```

Setting a key again records the new value and keeps the old row. A run that
changed policy mid-flight changed it, and that is history worth having.

**Put these lines in the orchestrator's own prompt template, not in a runbook.**
The orchestrator is the one process that exists before the fleet and outlives it,
and a step that lives only in a human's memory is a step that is skipped on the
run that most needs it. In the pattern below, the context rows go in at step 0 —
before the worktrees, because they describe the decision that chose the topology.

## The orchestrated-wave pattern

**0. The orchestrator records the run's constraints** (above), so the log it is
about to fill can be read back without guessing.


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
6. **The spawner declares `PACT_MODEL` and `PACT_HARNESS` beside `PACT_AGENT`**,
   so every row the agent writes can say what wrote it. See below.

## Declare what you launched

```bash
PACT_AGENT=agent-03 \
BEADS_ACTOR=agent-03 \
PACT_HARNESS=claude-code \
PACT_MODEL=sonnet-4-6 \
  claude -p "$PROMPT"
```

Two of these are new, and the argument for them is the argument for `BEADS_ACTOR`:
**the spawner is the only party that knows, at the only moment the knowledge is
free.** It just chose a model and a harness. The agent cannot reliably find out
what it is running, pact will not guess, and by the time anyone wants the answer —
reading `pact audit` after the run — the process is gone.

`PACT_HARNESS` is optional where pact can fingerprint the harness
([harness-detection.md](harness-detection.md) lists what it recognises, which
today is Claude Code and nothing else). `PACT_MODEL` is never optional, because
there is nothing to fingerprint: pact records a **declaration** and marks it as
one everywhere it renders it. A wrong declaration is worse than an absent one, so
declare what you actually requested and nothing else.

What you get: a `VIA` column in `pact lease ls` and `pact ui`, `model X (declared)`
in the per-agent audit table, a `models by events` line that shows a run meant to
be uniform that was not, and refusals that read

```
lease on src/api.rs is held by agent-01 [claude-code, sonnet-4-6] on branch wt/w3
```

instead of naming only `agent-01`.

### `PACT_HARNESS_SUBAGENT` — declare it, or leave it unset

`recount` can join a pact event to the exact harness transcript that produced it,
rather than inferring it topologically, when the event carries
`harness_subagent`. Like every other field here it is a **declaration**: pact
reads the environment variable and nothing else.

**Set it only if your harness or spawner tells you the id.** Some do; the one
pact can fingerprint today does not — measured on Claude Code 2.1.235
(2026-08-19), a spawned agent's environment is identical to its parent's and its
own id is nowhere in it, and the orchestrator does not know it at spawn time
either.

**Do not go looking for it on disk.** The id names a transcript file, so an agent
could in principle find its own by rummaging through the harness's state
directory — and that is exactly the sort of thing pact does not do and does not
ask you to do. It is a reverse-engineered private layout, it is one refactor from
breaking, and an agent reading its harness's internals to label its own log
entries is a coupling nobody wants to own. Absence is the honest signal.

Leave it unset and nothing is lost that was ever really there: `recount` uses the
topological join it has always used, and reports the confidence tier that says
which one it took. Never invent a value — a wrong key produces a confident join to
the wrong transcript, which is strictly worse than an honest inference.

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

## The self-merge mutex: `pact merge`

The reserved-key pattern above has one use common enough to have earned its own
command. A fleet with **no orchestrator** has nobody to merge for it, so every agent
merges its own worktree into the shared branch — and the shared branch has to be
serialized.

```bash
pact merge wt/wheelwright-millrace-ulk --verify 'cargo test --release'
```

That takes `.pact/internal/merge-to-<branch>` (derived from the branch you are on, so
on `master` it is exactly the key fleets were already spelling by hand), merges with
`--no-ff`, signs the merge commit, runs the verification, and releases.

**Why a command and not five lines of protocol prose.** The hand-rolled version works
— a four-agent run performed eight self-merges through it with `--check double-win`
clean over 63 events, and no merge ever happened unheld. Three things went wrong
anyway, none of them "the agents got it wrong":

- **`git merge` has no `--trailer`.** It has `--signoff` and nothing else. So an agent
  told to sign its commits with `git commit --trailer Pact-Agent=$PACT_AGENT` signs
  every commit it authors and *cannot* sign the merge. Measured: 13 work commits
  carried the trailer across five identities; all six merge commits did not, and
  `--check commit-correlation` exited 1 on precisely that. The one commit that changed
  the shared branch under a mutex — the commit the audit most wants attributed — was
  structurally the only unattributable one. `pact merge` merges with `--no-commit` and
  then commits, which is the only way a trailer reaches a merge.
- **The hold is a test run, not a lock.** Measured holds were 25–64s, median ~37s, and
  every second of that is the verification. Waiters should expect to wait that long,
  and the refusal now comes from the same code path as `lease acquire`, with the same
  holder-and-remaining reporting and the same exit 2.
- **The red path was untested prose.** "Revert, and *keep* the mutex until green" is
  the most dangerous instruction in the whole convention and the least often executed:
  it runs while the shared branch is broken and peers are blocked behind it. It is now
  code with a test. A failed verification resets the branch and **deliberately keeps
  the lock**, telling you so and naming the release command.

Two safety notes worth knowing before you use it:

- It **refuses a dirty tree** — tracked changes only; untracked files are fine. The red
  path resets `--hard`, and git would happily merge around unrelated dirty files that
  the reset then destroys. **Coordination state is exempt**: `.pact/`, `.beads/` and
  `.harness/` churn continuously in a shared checkout — a peer taking a lease, sending
  a notice or running any `bd` call writes to them — and treating that as a dirty tree
  refused every merge one agent attempted in a 12-agent run, 8 of 8. Those files are
  also *preserved across the revert*, because excluding them from the check without
  protecting them would just move the data loss: `reset --hard` would drop whatever
  peers appended while your merge ran. `--allow-dirty` overrides the guard for real
  work you have decided is safe to lose.
- `--verify` is optional but its absence is reported, never silently treated as a pass.
  `verified` is a three-state field in `--json` (`true`, `false`, `null`) for the same
  reason.

Checkpoint rotation — committing `.pact/events.jsonl` and `.pact/messages.jsonl` when
they have gone stale — is a natural chore for whoever holds this mutex, and is
deliberately **not** part of this command: committing files the caller did not name,
inside the one command that runs while a shared branch is half-written, is the wrong
place to be surprising.

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
