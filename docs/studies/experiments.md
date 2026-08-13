---
title: Synthetic experiments
description: The soak, the fault injector, the model checker and the property search — what each bounds, what each found, and why a green run alone proves nothing.
audience: contributors
---

# Synthetic experiments

The [field runs](field-runs.md) measure what agents actually do. They are bad at
proving pact is *correct*, because a real fleet does not contend on demand and
never crashes on cue. Four synthetic harnesses exist to bound the mechanics, so a
field run can be read as evidence about behaviour rather than about whether the
primitives hold.

| Harness | Question | Where |
|---|---|---|
| `scripts/fleet-sim.sh` | does coordination hold under contention? | [testing.md](../testing.md#the-fleet-soak) |
| `scripts/chaos.sh` | does a fleet *recover* when things fail? | [testing.md](../testing.md#fault-injection) |
| TLA+ / TLC | is the write-guard design right, not just its code? | below |
| Antithesis | which properties are worth asserting at all? | below |

The one thing they share is a discipline: **a passing treatment run is not
evidence unless the control fails.**

## The soak: a green run means nothing without a red one

`fleet-sim.sh` puts N scripted workers on one checkout doing read-modify-write on
shared files, and `fleet-verify.sh` checks no marker was lost. Its important flag
is `--no-leases`: the same workload with pact's primitive removed.

| 8 workers, 20 tasks | markers written | markers lost |
|---|---|---|
| leases off (control) | 39 | **9–10** |
| leases on | 39 | **0** |

The verifier treats a control that loses *nothing* as a harness failure, not a
success — because a clean treatment run on a workload that never contended says
nothing at all.

**Two calibration errors, both of which produced a passing result for a bad
reason.** They are the reason that rule is enforced in code rather than trusted:

- The read-modify-write window started at 0–0.2s. Workers spend *seconds* inside
  backend calls, so a 200ms window almost never overlapped another's and the
  control lost nothing. It is 0.1–2.0s now, tuned until the counterfactual
  actually failed.
- `--scope-local` looked like a perfect control — 26 of 26 markers "lost". It was
  the verifier reading the main worktree while each worker wrote into its own.
  `--scope-local` *cannot* produce lost updates: a worktree gives every worker its
  own inode for "the same path". What it does prove is that coordination
  disappears — exit-2 encounters drop to zero on a workload that produces them in
  shared mode.

### What the soak found on its first run

Not a bug in pact. Three of four exit-2 conflicts sent no message, and pact said
why:

```
note: you are yourself the last agent to work on notify/iface.txt; not adding a recipient
error: no recipients resolved — nothing to send
```

The workers had followed the protocol block's instruction to address the *file*.
But `--to-owner-of` resolves the **last agent to act** on a path, which is right
for a handoff and wrong at exit 2 — a worker that previously held and released
that path *is* the last actor, so it resolved to itself.

Two changes came out of it. A send whose every path resolves to the sender now
falls back to `human` instead of refusing. And the protocol block's two idioms
were separated by situation: for contention, `pact lease ls` names the *current*
holder, and `--to-owner-of` is the fallback for when they have gone. Conflicts
messaged went from 1 in 4 to 7 in 8.

**The general lesson:** the failure was diagnosable only because the worker
captured `msg send`'s stderr. The first version discarded it with `2>&1`, and a
harness that throws away the evidence for its own findings is half a harness.

### What the soak cannot prove

**It cannot tell you whether an agent understands the protocol.** These workers
were written *from* `AGENTS.md`, so they cannot misread it. Every finding about
comprehension — status pings, unread blockers, poll loops — came from the field
runs, and no scripted harness can produce one.

## Fault injection: recovery, not mechanics

The soak proves the primitives hold when everyone behaves. `chaos.sh` asks the
other question: an agent SIGKILLed with leases open, the Beads CLI gone from
`PATH`, a lock truncated to zero bytes, a lease backdated past its own TTL.

Nearly all of its code is blast radius rather than faults — five rails, refusing
to run outside a repo that carries both `.pact/` and a deliberate `.chaos-armed`
marker, never signalling a PID outside its allowlist, never renaming a binary
outside `$HOME`. Its own tests are almost entirely about the rails.

**Its three real defects were all in the rails, and all found by those tests:**

- **The PRNG returned a constant.** `$(rand_below 4)` runs in a subshell, so the
  counter advanced in the child and was discarded — five draws from one max all
  returned the same number. Every interval gap was identical and the plan varied
  only as the action pool shrank. It still *looked* seeded, because a different
  seed gives a different constant, which is precisely the failure mode the
  script's own header warned about two paragraphs above the bug.
- **The trap could not fire during an outage.** bash defers a handler until the
  running foreground command finishes, so a `TERM` during `sleep 90` was honoured
  90 seconds later — after the outage would have ended anyway. The rail claimed an
  outage never outlives chaos; chaos did not outlive its own sleep.
- **A `TERM` handler that only restored let the script continue**, so it put the
  binary back and re-hid it at the next planned outage. The signal looked handled
  and was not.

**SIGKILL is documented as the one limit rather than tested as if catchable.** The
kernel runs no handler, so the guarantee is "every signal a process can catch";
asserting recovery from SIGKILL would be asserting an impossibility.

The crucible run is chaos.sh pointed at a real fleet, and it exposed a coverage
bug in the injector itself: `stale-lock` drew one path, found a live agent holding
it, and left the pool — so the run planted **zero** stale leases. Every path worth
listing in a hint file is a hot path a busy fleet holds nearly all the time, which
made the highest-value fault the likeliest to no-op, and likelier the busier the
fleet was. Exactly backwards. It now walks the whole list, and the once-per-run
budget is spent when a fault *fires* rather than when it is scheduled.

## TLA+: verifying a design, not an implementation

One design here earned a model checker: the write guard that serialises the
takeover branches of `lease acquire`.

The problem is not expressible as a test. Two agents racing a takeover, one
crashing mid-write, a guard file that must not be reclaimed on a guess — the
interesting states are interleavings a test can only sample. TLC explored roughly
**76,000 states** of a crash-gated reclaim model and the design survived; the
implementation was then written against the invariants the spec named.

The division of labour is the useful part:

| | catches |
|---|---|
| TLA+ | protocol deadlocks, consistency violations under failure, missing error paths, algorithmic races |
| tests / PBT | off-by-one, serialization, edge cases in data transforms |

Neither catches performance, usability, or integration with a real `bd`. And a
verified design says nothing about the code unless the invariants are carried
across deliberately — which is why the guard's doc comment names the property it
is implementing rather than describing the mechanism.

## Antithesis: choosing what to assert

A property-search pass over pact produced **57 candidate properties**, triaged
into **74 tracked issues**. Its value was not the assertions; it was the forced
question *what would falsify this*, applied to every claim pact makes about
itself.

Two habits survived into the everyday workflow:

- **A claimed guarantee becomes a property to test, not a fact to state.** If a
  doc comment says "this cannot happen", that sentence is a test case.
- **A bug report is a lead, not a fact.** Confirm the defect is real before
  building on it — several candidate properties dissolved on inspection, and one
  turned out to be describing the reporter's environment.

## Why all four exist

Each one bounds a different failure of the others:

- the soak cannot crash anything → `chaos.sh`
- `chaos.sh` samples interleavings → TLA+ exhausts them
- all three test what somebody thought to test → the property search asks what was
  not thought of
- none of them can tell you what an agent will actually *do* → the
  [field runs](field-runs.md)

A finding from any one of them is worth a bead. A green run from all four is worth
considerably less than one field run's `.pact/events.jsonl`, which is why the
studies are ordered the way they are.
