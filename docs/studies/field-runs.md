---
title: Field runs
description: Four repositories built by agent fleets on pact, what each run measured, and the design decisions the numbers forced.
audience: contributors
---

# Field runs

Four repositories have been built by agent fleets coordinating through pact.
None of them are pact. Each produced a committed `.pact/events.jsonl` that can be
re-audited today, and each changed pact in ways nobody predicted from a
whiteboard.

| Run | Repository | Fleet | Events | What it was for |
|---|---|---|---|---|
| 1 | [arkanoid](https://github.com/chussenot/arkanoid) | 8 agents, unorchestrated | 52 | first field data of any kind |
| 2 | [megablast](https://github.com/chussenot/megablast) | 20 agents, 5 waves, 63 min | 62 | does an orchestrated topology help? |
| 3 | [grimcast](https://github.com/chussenot/grimcast) | 23 agents, long-running | 263 | first run with `pact watch` live |
| 4 | [crucible](https://github.com/chussenot/crucible) | 10 agents, free-running, 85 min | 352 | deliberately hostile: contention and faults |

This page is the evidence. The rules it produced live in the reference docs —
[leases.md](../leases.md), [messaging.md](../messaging.md),
[watch.md](../watch.md), [audit.md](../audit.md) — which state each rule and its
one-sentence reason and link back here for the numbers.

## How to read a number on this page

Every figure below came from `pact audit` or `git log` over a committed log, not
from an agent's self-report. That distinction is not pedantry: an early analysis
of run 2 concluded the fleet had sent **no** messages, on the strength of a
report, when it had sent four and one of them prevented a shipped bug.

**Three counting hazards, each of which has produced a wrong published claim:**

- `.beads/interactions.jsonl` **does not count messages.** It reads zero
  `pact-msg-` references for arkanoid and megablast, both of which demonstrably
  sent messages. It records some bd operations, not bead creation, so a grep over
  it undercounts silently — which is the worst way for a counter to be wrong. Use
  `bd list --type=message` against the run's store.
- **`jq -r 'select(…)' | wc -l` counts pretty-printed lines, not matches.** It
  once turned 22 assignee changes into 264. Use `jq -s length`, or `jq -c`.
- **A field that does not move is not proof the behaviour did not happen.** Claim
  adherence was published as "not measurable in either direction" because bd
  writes a `field_change/status` interaction only on close. But bd logs *assignee*
  changes too, and a self-claim has a distinctive shape — so it was measurable all
  along through a field nobody had looked at:

  | run | assignee interactions | status/closed | claim adherence |
  |---|---|---|---|
  | megablast | 22 | 23 | **22/23** |
  | grimcast | 0 | 22 | **0/22** |

  grimcast regressed to zero claims while closing every bead. The original wrong
  result came from testing `--claim` on a scratch store where it was a **no-op**
  (it is idempotent, so re-claiming your own issue logs nothing) and generalising
  from that.

Where a number here could not be re-derived from a committed artifact, it says so
rather than being quoted as measured. **A metric that returns the same answer
regardless of the behaviour it claims to measure is worse than no metric, because
it looks like evidence** — and so is concluding a thing cannot be measured because
the first field you checked did not move.

## Run 1 — arkanoid: the baseline nobody had

8 agents, no orchestration, 52 events. Its value is entirely as a control: it is
what pact looked like when nothing about the topology was deliberate.

What it produced, and what changed:

- **One 6,167-line commit** for the whole build, under **one** git identity, with
  **8 agents contending on a single file.** There was no way to attribute any
  line of it to any agent. This is the measurement the orchestrated-wave pattern
  in run 2 was designed against.
- **51 of 59 messages were never read**, because they were addressed to agents
  that had already exited. Addressing was never the failure — deliverability was.
  Hence `msg send --to-owner-of <path>`: a message tagged with a path is offered
  to whoever leases that path next, so it outlives the process it was aimed at.
- **3 messages sent, 2 of them read by an agent other than the addressee.** That
  is `--to-owner-of` working as designed rather than a defect, and noticing the
  difference is why `pact audit --export` reports `unacknowledged_messages` with
  "nobody read it" kept distinct from "read by somebody who was not the
  addressee".

Filed as `pact-juz`.

## Run 2 — megablast: orchestration, measured

20 agents, 5 waves, one git worktree per agent per wave, 63 minutes. The
[orchestrated-wave pattern](../fleet-patterns.md) is this run written down.

Against run 1:

| | arkanoid | megablast |
|---|---|---|
| commit provenance | one 6,167-line commit | 62 commits, 20 of them wave merges |
| bd actor attribution | 1 collapsed git identity | 21 distinct actors |
| worst-case contention | 8 agents on one file | 3 holds on the busiest path |
| lease hygiene | — | 31/31 clean releases, 0 stale holds, longest 10m30s against a 45m TTL |

**Designing the module tree turned out to be fleet planning.** The contention
spread was not luck — the file layout was chosen so concurrent agents would own
different files.

Three decisions came out of it:

- **Commit before you release.** A fix landed **99 seconds after** its author had
  already released the file it changed — one of 19 commits touching a leased path
  outside every recorded hold for it. Releasing first breaks the binding the log
  exists to prove, and
  [`--check commit-correlation`](../audit.md#--check-commit-correlation) is the
  detector.
- **pact does not see your worktrees unless you run it in them.** Every lease in
  the run was taken from the *main* checkout while editing happened in worktrees,
  because agents inherited the orchestrator's working directory. It worked —
  repo-relative keys make the namespace correct either way — but the lease/edit
  binding rested on convention rather than record. Hence `invoked_from` on every
  event, and [`--check topology`](../audit.md#--check-topology---expect-worktreesmainany).
- **Zero `refused` events across 20 agents.** Wave scheduling really does
  pre-resolve contention. That zero is what kept the contention-communication
  check parked for months: a check whose input never occurs earns nothing. Run 4
  finally produced the input.

**One message, and it was load-bearing.** An agent that had changed a constant in
a file it owned told the owner of a *different* file which term to update, naming
the exact failure — a `write_buffer` overflow once background instances exceeded
the buffer size. The fix landed. Had it not been sent, the bug ships. Every claim
about messaging being unnecessary has to survive this one message, and none of
them do.

Filed as `pact-ler`.

## Run 3 — grimcast: the first run with `watch` live

23 agents, 263 events, and the run that made `pact watch` real rather than
theoretical: **87 diffs delivered.**

- **The size cap was wrong, measured.** It was 200 lines, chosen before any field
  data existed, and this run truncated **44 of 87** delivered diffs — whose real
  sizes were median 397 lines and largest 839, nowhere near the size the cap was
  imagined for. Raised to 1000, which delivers every diff this run produced in
  full. Truncation costs more for an agent than a human: a cut diff degrades to
  "go run `git show`", a second step off the critical path, which is the exact
  category of voluntary step this feature exists because agents skip.
- **33 hand-written messages, and 0 of them replied into the notification that
  prompted them** — even though four were explicit acknowledgements of a received
  diff and others adapted to one. The notification body's only call to action was
  how to *unsubscribe*: the one instruction pact gave a subscriber at the moment
  they had a question was how to stop hearing from you. It now says how to answer
  first and how to leave second.
- **The protocol and the binary both changed mid-run**, because a pact release
  was cut while the fleet was working. Three-way distinction the analysis had to
  make: the binary changed (true), the block pact *would write* changed (true),
  but the block the agents actually *read* did not — `AGENTS.md` was last touched
  23 minutes before the first event and stayed clean. Hence `pact_version` and
  `protocol_hash` on every event, so a run that straddles an upgrade says so
  instead of being reconstructed by hand.
- **90 commits, 90 under one git author**, against 23 distinct agent identities.
  Agent identity does not survive into git, which is why
  [commit attribution](../audit.md#attribution-and-why-covered-was-answering-the-wrong-question)
  needs a trailer rather than an inference.

Filed as `pact-b73`.

## Run 4 — crucible: built to hurt

10 agents, free-running with no wave gating, 24 beads with deliberate file
overlap on one hot AST file, `scripts/chaos.sh` injecting faults on seed 1337,
and one agent told the protocol did not apply to it. 352 events, 85 minutes.

It is the only run that exercised the half of pact the first three never touched:

| | runs 1–3 combined | crucible |
|---|---|---|
| refusals | 0 | **124** |
| takeovers | ~0 | 5 |
| expiries | 0 | 9 |
| renewals | ~1 | 14 |
| injected faults | 0 | 3 SIGKILLs, 3 backend outages, 1 vandalised lock |

**pact came through well.** Chain integrity verified across all 352 lines. Leases
proved genuinely backend-independent and kept working through every `bd` outage.
The corrupt-lock path detected, recovered and carried provenance forward in 30
seconds. Refusals were informative and agents acted on them correctly.

The findings are the ones only visible once takeover, crash and non-compliance
are all in play at once:

- **A forced `--steal` never closed the victim's hold**, so one SIGKILLed holder
  poisoned the rest of the log: 9 reported double-wins, 8 naming one dead agent,
  **zero** of which were two live agents holding a path. The count grew with
  traffic rather than with risk. Fixed by closing the displaced window at the
  steal and splitting expected takeovers from real double grants — 9 findings
  became 3, and the 3 are exactly the genuine forced steals.
- **A lease is exclusive in time, not across copies.** Two agents each correctly
  held one file at different times, each added a match arm to the *same* match
  statement on its own branch, and git merged both cleanly because they were
  textually non-adjacent. Duplicate arms, six duplicate test functions (E0428),
  and a near-miss where applying a stashed diff would have silently reverted a
  peer's change. **No conflict marker was ever produced in any of them.** Caught
  by a compile failure, never by pact. Hence the content-hash comparison at
  acquire and [`--check merge-divergence`](../audit.md#--check-merge-divergence).
- **Two locks that never consulted each other.** `bd update --claim` decides who
  owns the work; `lease acquire` decides who may edit the files. An agent that
  *lost* the bead claim was still granted every lease that bead named, verbatim
  from the run: *"a race let me briefly hold src/main.rs+src/lib.rs for
  crucible-2o3.27 after `bd update --claim` had already lost that bead to
  agent-07."* It self-corrected only because an agent noticed.
- **A stalled holder is worse than a crashed one.** Seven of ten agents ended
  their turn early waiting on a poller that could not wake them; one sat stopped
  for minutes *while holding* `src/printer.rs`, which `lease ls` called `active`
  with a live holder. **It cost more fleet time than every injected fault
  combined.** Hence the `SUSPECT` liveness column.
- **`release` reported success on a lease that had already lapsed.** An agent's
  lock was collected at 09:12:32Z; it committed at 09:14:01Z, released, and was
  told it had released cleanly. For 90 seconds the path was free and any peer
  could have taken it. It found out by reading `events.jsonl` afterwards.
- **The rogue agent's worst commit was invisible.** It authored zero of the 352
  events and committed freely; `--check commit-correlation` flagged three
  uncovered commits, one genuinely its. But its five-file unleased commit passed
  clean, because at that instant every one of those five paths was under an
  active lease held by a *compliant peer*. **The better the rest of the fleet
  behaves, the better a rogue hides.** Merging that branch then needed a hand
  three-way reconciliation, detected ~13 minutes later by a human running `git
  merge`.

Filed as `pact-mqw`. A second pass over the same log, once contention data
existed to look at, filed `pact-1gv`:

- **Retry spacing was 15 seconds flat.** One agent refused `src/eval.rs` **33
  times**, and 27 of its 32 gaps were *exactly* 15 seconds — a hardcoded poll
  loop, not adaptive retry. Against a median advertised remaining hold of **355
  seconds**, it retried roughly 24× more often than the number in its own
  refusal message told it to, and the holder's note, quoted back every time, said
  "LONG BEAD, will renew". Hence [`--check retry-storm`](../audit.md#--check-retry-storm).
- **24 refusals came from an agent that had already subscribed to the path.** One
  polled 13 times after subscribing. The notification channel was armed and the
  agent polled anyway, because nothing at refusal time told it so. Hence the
  refusal now saying "you already watch this — do not poll".
- **The refusal's holder facts were prose only.** The one structured number that
  looked like an answer was the wrong one: `ttl_secs` on a refusal is the ttl the
  *refused* agent asked for, and read 600 on all 33 of those refusals while the
  holder's remaining ranged 96–597s. Hence `holder_remaining_secs` as a field.

## What the four runs actually say about messaging

This is the claim that has been got wrong more than any other on this page, in
both directions, and once in a doc that shipped.

The measured picture:

| Run | Agent-to-agent messages | Watch deliveries |
|---|---|---|
| arkanoid | 3 | — (watch did not exist) |
| megablast | 1 | — |
| grimcast | 33 | 87 |
| crucible | ~0 | 64 |

**A published claim that "four fleet runs produced zero voluntary
agent-to-agent messages" was wrong**, and this table is why. It was taken from a
report premise and "verified" with a grep over `.beads/interactions.jsonl`, which
returns zero even for runs that provably sent messages. grimcast alone sent 33.
The correction is recorded here rather than quietly patched, because a docs set
whose provenance section is itself unsourced has no claim on anyone's trust.

What the numbers *do* support:

- **Messaging is bimodal under prose, not absent.** Unrestrained, pact's own
  fleet sent 223 messages, 41 of them status pings in a single run — see
  [dogfooding.md](dogfooding.md). Restrained by the protocol block, arkanoid and
  megablast sent 3 and 1. grimcast's 33 shows the volume returns when a run is
  long and interfaces churn.
- **Watch delivered without being asked**: 87 and 64, in runs where hand-written
  messaging was 33 and ~0. Delivery rides `lease release`, a command those runs
  performed 31 times out of 31, so it costs nothing at announce time.
- **The load-bearing messages are a tiny minority and cannot be predicted.** One
  message across runs 1–2 prevented a shipped bug. Any rule that suppresses
  volume suppresses that one too.

So the defensible design position is not "agents do not message" but: **make the
mechanical announcement structural, and leave messaging for what needs an
answer.** That is what `pact watch` is, and it is why the protocol block reserves
messages for things that need something back rather than telling agents to
message less.

## The backlog this produced

Every finding above became a bead with a citation. Closed epics, newest first:

| Epic | Run | What it covered |
|---|---|---|
| `pact-1gv` | crucible | retry storms, contention efficiency, silent contention, refusal fields |
| `pact-mqw` | crucible | steal/victim windows, worktree divergence, claim-vs-lease, liveness, release honesty, commit attribution |
| `pact-b73` | grimcast | protocol/binary versioning, the diff cap, notification ordering, `head` stamping |
| `pact-ler` | megablast | `invoked_from`, topology check, `--export`, actor attribution |
| `pact-juz` | arkanoid | lease event logging, duplicate detection, `--to-owner-of` delivery |

Two remain open, both deliberately: `pact-b73.6` (teach commit-correlation to use
the recorded `HEAD` range) waits on a run whose events carry `head`, and
`pact-07a` is a test-harness flake.

## What no field run can tell you

Whether an agent recovered *well*. These runs record what pact did and what the
fleet did in response; whether the response was sensible is a judgement a human
makes by reading both logs side by side. The synthetic
[experiments](experiments.md) exist to bound the mechanics so that the field runs
can be read as evidence about behaviour rather than about pact's correctness.
