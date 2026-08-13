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
| Fault injection | [`scripts/chaos.sh`](../scripts/chaos.sh) | does a fleet RECOVER when agents and tools fail |

The first three are in [development.md](development.md). This page is the last two.

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

Three of four exit-2 conflicts sent no message, because the workers had followed
the protocol block's instruction to address the *file* — and `--to-owner-of`
resolves the **last agent to act** on a path, which at exit 2 is often the asker
itself. Two fixes came out of it, and one general lesson: the failure was
diagnosable only because the worker captured `msg send`'s stderr, and a harness
that discards the evidence for its own findings is half a harness.

The full account is in
[studies/experiments.md](studies/experiments.md#what-the-soak-found-on-its-first-run).

## Fault injection

The soak proves the primitives hold when everyone behaves. Twenty workers race
one checkout, every one of them following the protocol, and the verifier checks
that no two ever held a path at once. That is the mechanics question.

`scripts/chaos.sh` asks the other one: **does a fleet recover when things
fail?** An agent SIGKILLed with three leases open. The Beads CLI gone from
`PATH` for ninety seconds. A lock file truncated to zero bytes. A lease
backdated past its own TTL so two agents both believe they can take it.

```bash
touch /tmp/fleet/.chaos-armed              # deliberate, and required
scripts/chaos.sh --repo /tmp/fleet --pids /tmp/fleet/pids --dry-run
scripts/chaos.sh --repo /tmp/fleet --pids /tmp/fleet/pids --seed 7 --duration 30
scripts/chaos.sh … --time-unit sec         # duration AND gaps in seconds
```

`--pids` is a file the orchestrator appends `pid<TAB>agent` to as each agent
spawns. It is the allowlist, so an agent that is not in it cannot be killed —
which also means an orchestrator that forgets to register its agents gets a run
full of logged skips rather than a run full of faults, and the log says which.

`--time-unit sec` scales the duration *and* the gaps. It exists so the tests can
drive real faults end to end in seconds; `mise run check` runs them on every
gate, and `shellcheck --severity=warning scripts/*.sh` runs there too, because
"shellcheck-clean" for a destructive script has to be enforced rather than
claimed.

Sim and chaos answer different questions and neither substitutes for the other.
A fleet can pass the soak and still lose work the first time an agent dies
holding something — the soak has no dying agents in it.

### It must never run outside a disposable fleet repo

**This script kills processes and rewrites files it did not create.** Almost all
of its code is the blast radius rather than the faults; each fault is about a
dozen lines. Five rails, in the order they fire:

| Rail | What it refuses |
|---|---|
| pact's own checkout | `--repo` resolving to pact's canonical path, or to anything with `src/lease.rs` and a `Cargo.toml` naming the pact package |
| two markers | a repo without **both** `.pact/` and `.chaos-armed` |
| PID allowlist | signalling any PID not listed in `--pids`, re-verified per kill |
| cwd re-check | signalling a listed PID whose working directory is not under `--repo` |
| `$HOME` prefix | renaming a backend binary that lives outside `$HOME` |

Two of those deserve their reasons stated. `.pact/` alone is **not** sufficient,
because every repository pact has ever touched has one — including this one — so
a single marker is one typo away from somebody's work. And the PID allowlist is
re-verified *before every signal* rather than once at startup, because PIDs are
recycled: an entry naming a process that has since exited can name something
else entirely by the time chaos reads it, which is exactly how a bounded tool
becomes unbounded.

The `$HOME` rail is why `backend-outage` skips rather than fails on a
system-installed `bd`. Hiding `/usr/bin/bd` would break the machine, not the
fleet.

### Restore is a trap, and its one limit is documented

`backend-outage` renames a binary, so putting it back cannot be a code path that
the happy case reaches — it is a `trap` on `EXIT INT TERM`. An outage must never
outlive the process that caused it.

**SIGKILL is the exception, and unavoidably so.** The kernel delivers it without
running any handler, so a `kill -9` of chaos mid-outage leaves the binary
renamed. The guarantee is "every signal a process can catch", not "every
signal", and the test asserts the former because asserting the latter would be
asserting an impossibility. If it does happen, the fix is one `mv` and the log
names the file.

### The join contract

Every decision appends one line to `<repo>/chaos-log.jsonl`:

```json
{"ts":"…","seed":7,"action":"kill-holder","target":"w3-combat pid=4131","detail":"SIGKILL sent; held: src/game/mod.rs","dry":false}
```

**Including skips and refusals**, which is the point of the file rather than a
nicety. An analysis joining effects to causes also has to join every *non*-effect
to the rail that prevented it — otherwise "chaos did nothing here" cannot be
told apart from "chaos tried and a rail stopped it", and a rail that fires
silently is a rail nobody can audit.

### Once per run means once *fired*, not once attempted

`lock-vandal` and `stale-lock` land at most once in a run. That budget is spent
when the fault actually executes, not when the planner schedules it — and the
difference cost a whole run's worth of coverage before it was fixed.

A real run caught this: `stale-lock` drew one path, found a live agent holding it,
and left the pool — so the run planted **zero** stale leases. Every path worth
listing in a hint file is a hot path a busy fleet holds nearly all the time, which
made the highest-value fault the likeliest to no-op, and likelier the busier the
fleet was. Exactly backwards
([evidence](studies/experiments.md#fault-injection-recovery-not-mechanics)).

Two changes, together:

- **`stale-lock` walks the whole hint list**, in a seeded shuffle, and plants on
  the first path it can acquire. A path it cannot take is one logged skip, not the
  end of the action. Exhausting the list is its own logged verdict, so "chaos
  planted nothing" always says why.
- **The gate moved from the planner to the dispatch loop.** A once-per-run action
  stays in the draw pool, so a long run may offer it several slots; the first slot
  that *fires* spends the budget and later ones are logged as spent.

The cost is that a spent slot is a logged skip rather than a fault. Paid
deliberately: a wasted slot is visible in the log, and a fault that never fired
because it got one unlucky draw is not.

### Reproducibility

Randomness comes from `sha256("<seed>:<counter>")`. Not `$RANDOM`, which cannot
be seeded across processes, and deliberately not `awk`'s `srand()` either: that
one *is* seedable, but its generator is implementation-defined, so mawk, gawk and
busybox awk produce different sequences from the same seed. A plan built with it
would reproduce on the machine that found a bug and nowhere else — worse than
obvious non-determinism, because it looks reproducible until it is not.

The whole fault plan is computed before anything is touched, so `--dry-run`
emits exactly the sequence of faults a real run would attempt. That is also what
CI runs: a dry pass is a full self-test of the planner and every rail, with no
side effect but its own log.

It prints attempts, not outcomes — a dry run cannot know that a PID will have
exited or that a peer will be holding a path, so it reports what an unobstructed
run would do. A real run's skips are where the two diverge, and each one says
which rail or which peer accounts for it.

A run given no `--seed` picks one and logs it like any other decision, because a
fault injector whose failures cannot be replayed is the one thing it must never
be.

### What it cannot tell you

Nothing about whether a language model recovers well. chaos breaks things and
records what it broke; whether the fleet then did something sensible is a
question for `pact audit` over the same window, and ultimately for a human
reading both logs side by side. The `--seed` and the timestamps are what let you
line them up.
