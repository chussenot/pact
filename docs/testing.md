---
title: Testing pact
description: The fleet soak — scripted workers at concurrency — and what it cannot prove.
audience: contributors
---

# Testing pact

Four layers, each answering a question the one below it cannot.

| Layer | Where | Answers |
|---|---|---|
| Unit | inside each module | does this function do what its name says |
| Integration | `tests/*.rs` | does the real binary behave, over real pipes and real git |
| Canary | [`scripts/canary.sh`](../scripts/canary.sh) | do pact's assumptions about somebody else's CLI still hold |
| Fleet soak | [`scripts/fleet-sim.sh`](../scripts/fleet-sim.sh) | do the primitives hold when twenty agents contend |

The first three are in [development.md](development.md). This page is the fourth.

## The fleet soak

Every earlier finding about pact under load came from watching real agent fleets:
the 85-message run where 41 were status pings, the 51-of-59 unread messages, the
lease that printed `3520s` and got force-released. That is expensive, slow, and
unrepeatable — you cannot re-run a fleet to check whether a fix worked.

`scripts/fleet-sim.sh` replaces the agents with shell workers that do
*mechanically* what `AGENTS.md`'s managed block tells a real agent to do. Then
concurrency is a flag, a run is a minute, and CI can do it weekly.

```bash
mise run fleet                              # the control/treatment pair
scripts/fleet-sim.sh -n 20 -t 60            # 20 workers, 60 tasks
scripts/fleet-verify.sh /tmp/pact-fleet-XXX # assert on the run
```

`fleet-sim.sh` prints its run directory as the last line; hand that to
`fleet-verify.sh`. They share nothing but the `manifest.json` in between.

### What a worker does

Straight from the protocol block, in order:

1. `pact msg inbox` and `pact lease ls` **before** touching a file
2. take a ready task and claim it — `bd update --claim` is documented atomic, and
   a second claimer is refused, so two workers never work one task
3. `pact lease acquire <all targets> --note …` — one call, all-or-nothing, so a
   worker never holds half of what it needs
4. on **exit 2**: find the holder, message them, put the task back, take another
5. read-modify-write each target, then hold a moment
6. `git commit`, `pact lease release --all`, close the bead

Workers branch **only on documented exit codes**, never on message text. That is
deliberately part of what is under test: if pact ever returns 1 where it returned
2, the workers stop coordinating and the verifier notices.

### What the verifier asserts

**No lost updates.** Every marker a worker *logged as written* is present in the
file. Checked against the workers' own logs, not against a count — "the file has
40 markers" proves nothing without knowing 41 were written. In `--worktrees` mode
each marker is checked in that worker's own checkout, which is a detail worth
stating because getting it wrong made the control group pass for the wrong reason
(see below).

The write is a **read-modify-write**, not an append, and that choice is the
experiment. A single small `>>` is atomic on Linux and would survive with no
lease at all, proving nothing. The failure a lease actually prevents is the *lost
update*: two workers each read a file, each write back what they read plus their
own line, and one line vanishes.

**No double-win.** Hold windows are reconstructed from `.pact/events.jsonl` —
`acquired` and `stolen` open one, `released`/`force-released`/`expired` close one,
`renewed` is neither. Two agents open on one path at once is a finding.

Finding one is a **success of this harness, not a failure to hide**. It is the
written trigger condition for the guard-file backlog item (`pact-ehi`), which says
implement the guard file *if and only if* a double-win appears in a real events
log. So the verifier exits 1 with both agents, both timestamps and the event ids,
and says where the evidence belongs.

**Message round-trip.** Every exit-2 encounter produced a message that reached
somebody's inbox — asked through `pact msg inbox` rather than the backend,
because bd hides message beads without `--include-infra` and br has no such flag,
argv differences pact already encapsulates.

One class of miss is accepted, and pretending otherwise made the assertion
unachievable rather than strict: if the holder *released* between the exit-2 and
the lookup there is nobody current to name. Nobody to tell is not a failure to
tell — the worker's real obligation at exit 2 is to go find other work.

**Liveness.** Every task closed, and no lease outlived the fleet. Lease ordering
can deadlock a fleet — two workers each holding one of two paths the other
needs — and all-or-nothing multi-path acquire is what prevents it.

### The modes, and which one makes the rest mean anything

| Flag | What it exercises |
|---|---|
| *(none)* | one shared checkout, leases on — the main case |
| `--worktrees` | a linked worktree per worker: shared resolution under load |
| `--steal-storm` | every worker starts by racing already-expired leases |
| `--no-leases` | **control**: same workload, pact's primitive removed |
| `--scope-local` | **control**: `PACT_WORKTREE_SCOPE=local`, so coordination is absent |

`--no-leases` is the load-bearing one. A green treatment run on its own says
nothing — the workload might simply never have contended. So the control must
**fail**, and `fleet-verify.sh` treats a control run that loses *nothing* as a
failure of the harness:

```
control lost 10 of 39 markers, as it must — the workload really contends,
so a clean run with leases is a result rather than an accident
```

Measured on the same workload, 8 workers and 20 tasks:

| | markers written | markers lost |
|---|---|---|
| leases off (control) | 39 | **9–10** |
| leases on | 39 | **0** |

Two things had to be got wrong before that pair was trustworthy, and both are
worth knowing because both produced a *passing* result for a bad reason:

- The read-modify-write window started at 0–0.2s. Workers spend *seconds* inside
  backend calls, so a 200ms window almost never overlapped another worker's and
  the control group lost nothing. It is 0.1–2.0s now, tuned until the
  counterfactual failed.
- `--scope-local` looked like a perfect control at first — 26 of 26 markers
  "lost". It was the verifier reading the main worktree while each worker wrote
  into its own. `--scope-local` **cannot** produce lost updates: a worktree gives
  every worker its own copy of every file, so "the same path" is a different inode
  for each of them and no lease could matter. What it does prove is that the
  *coordination* disappears — exit-2 encounters drop to zero on a workload that
  produces them in shared mode.

## What this cannot prove

The limit is not a caveat, it is the shape of the tool.

**It cannot tell you whether an agent understands the protocol.** These workers
were written *from* `AGENTS.md`, so they cannot misread it. Every real finding
about protocol comprehension — that 41 of 85 messages were status pings nobody
could triage, that agents addressed peers who had already exited, that one agent
leased both files it edited and still corrupted a shared store because it read
"lease what you edit" as being about editing — came from an LLM doing something a
shell script would never think to do. This harness would have caught none of it.

So it proves the **primitives** hold under contention, and says nothing about
whether the protocol *around* them is understandable. Those need real fleets, and
the evidence habit in the README is still how they get found.

Narrower limits, all of them real:

- **Probabilistic.** A double-win might need a hundred runs to surface. A clean
  run is evidence, not proof; that is why the trigger condition in `pact-ehi` is
  "if one ever appears" rather than "if the soak passes".
- **It does not test the backend under concurrency.** Task claiming leans on
  `bd update --claim` being atomic, which was verified by hand and is assumed
  here. `canary.sh` is where backend behaviour is under test.
- **One machine.** Everything is one filesystem, which is pact's scope anyway
  ([architecture.md](architecture.md#what-pact-deliberately-doesnt-do)).
- **The synthetic codebase is not code.** Twenty text files across four modules,
  with a per-module `iface` file many tasks touch. The overlap is the point; the
  contents are irrelevant.

## In CI

[`.github/workflows/fleet.yml`](../.github/workflows/fleet.yml) runs all five
modes weekly, plus on demand. Not on pull requests, and not a required check: it
is minutes long, probabilistic, and depends on somebody else's release process —
each of those on its own turns a required gate into one people disable, which is
the same reasoning that keeps the canary off PRs.

Failure uploads the worker logs, the manifest, the lost-update list, the
double-win forensics and the event log as artifacts, then files or comments on one
`fleet-soak` issue. Which mode failed is the diagnosis:

| Failing mode | Meaning |
|---|---|
| `control` alone | the harness stopped being a test — nothing else passing means anything |
| `shared` | pact stopped protecting the invariant leases exist for |
| `worktrees` | shared resolution broke under load |
| `steal-storm` | the expired-lease takeover path broke |
| `scope-local` | isolation stopped isolating |

## A finding the harness produced on its first run

Worth recording, because it is the kind of thing the harness exists for and it is
not a bug in pact.

Three of four exit-2 conflicts sent no message, with what pact printed at the
time:

```
note: you are yourself the last agent to work on notify/iface.txt; not adding a recipient
error: no recipients resolved — nothing to send
```

The workers had used `pact msg send --to-owner-of <path>`, following the protocol
block's instruction to address the *file* rather than the name. But
`--to-owner-of` resolves the **last agent to act** on a path from the event log,
which is right for a handoff and wrong at exit 2: a worker that previously held
and released that path *is* the last actor, so it resolved to itself, and pact
then treated a self-resolution as no recipient at all and refused the send.

That second line is gone — a send whose every `--to-owner-of` path resolves to
the sender now falls back to `human` and still tags the path
([why](messaging.md#when-every-path-resolves-to-you)). The finding stands
regardless: the message the worker wanted to send was for whoever holds the path
*now*, and no addressing trick reaches them from the event log.

The protocol block contains both idioms, and for contention the other sentence is
the correct one — "`pact lease ls` names the holder; message them". The workers do
that now, with `--to-owner-of` as the fallback for when the holder has already
gone. Conflicts messaged went from 1 in 4 to 7 in 8, the remainder being the
benign already-released race.

Two lessons, one general: the failure was diagnosable only because the worker
captured `msg send`'s stderr. The first version discarded it with `2>&1`, and a
harness that throws away the evidence for its own finding is half a harness.
