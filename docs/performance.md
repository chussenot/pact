---
title: Performance
description: What the lease hot path costs, measured; where the time actually goes; and the CI budget that protects the order of magnitude.
audience: contributors
---

# Performance

pact's performance claim has always been a *design* claim: coordination is files
on one filesystem, and the only part that shells out to another program —
messaging, via the Beads CLI — is deliberately not on the lease path.

**The first half is measured now, and it is not what the claim implied.** The
Beads part holds: nothing in `lease acquire`/`release` can reach `bd`. But the
lease path is not filesystem-bound either — it is bound by **subprocesses pact
spawns itself**, `git rev-parse` and `git hash-object`, plus a parse of its own
event log. The lock file is microseconds; everything around it is milliseconds.

Two of those costs have since been reduced (see
[what was optimised](#what-was-optimised-and-what-was-left-alone)) and a third was
rejected because the measurement said it would buy nothing. The numbers below are
after that work.

Everything below comes from `benches/lease.rs` under criterion, against a
tempdir repo with **no `bd` on `PATH`**. Reproduce with `mise run bench`.

## The numbers

Reference hardware: the figures below were taken on one Linux x86-64 developer
machine with an SSD. **Treat the ratios as the result and the absolute values as
that machine's**, which is also why the CI budget carries ~5x headroom rather than
tracking these to the percent.

Every repo is seeded to 4500 event-log lines first — between the trim floor
(4000) and cap (5000) that `events.rs` documents — so each benchmark measures a
repository that has been in use, and the numbers are comparable to each other.

| Benchmark | ns/op | ops/sec | What it is |
|---|---|---|---|
| `repo/resolve/plain` | 5 573 | 179 438 | topology resolution, ordinary checkout |
| `repo/resolve/plain_with_worktrees` | 7 780 | 128 527 | …plus the `worktrees/` scan |
| `repo/resolve/linked_worktree_commondir` | 30 690 | 32 584 | …the commondir walk, from a linked worktree |
| `events/append/head_cache/warm` | 402 170 | 2 487 | a boundary append reusing a resolved HEAD |
| `events/append/head_cache/cold` | 2 774 912 | 360 | the same append, resolving HEAD itself |
| `events/append/by_kind/notified` | 397 279 | 2 517 | an append that stamps no `head` at all |
| `events/append/by_kind/acquired` | 3 098 162 | 323 | the same append, cold, stamping `head` |
| `events/append/0` | 393 027 | 2 544 | warm append, empty log |
| `events/append/1000` | 395 312 | 2 530 | warm append, 1000 lines |
| `events/append/4500` | 414 795 | 2 411 | warm append, steady state |
| `lease/refresh_reentrant` | 526 497 | 1 899 | re-acquire your own live lease |
| `lease/acquire_contended_exit2` | 626 058 | 1 597 | the refusal path an agent hits at exit 2 |
| `lease/release` | 4 077 754 | 245 | release a lease you hold |
| `lease/acquire_clean` | 8 727 871 | 115 | claim a path that does not exist yet |
| `lease/with_subprocess/acquire_hashes_an_existing_file` | 6 046 738 | 165 | claim a path that **does** exist — the common case |
| `lease/roundtrip_acquire_release` | **20 330 521** | **49** | **acquire + release of an existing file — the budgeted number** |
| `lease/acquire_many/1` | 8 892 416 | 112 | batch of 1 |
| `lease/acquire_many/5` | 23 632 565 | 42 | batch of 5 (4.7 ms/path) |
| `lease/acquire_many/20` | 85 924 804 | 12 | batch of 20 (4.3 ms/path) |

Each benchmark models **one CLI invocation**: the HEAD cache below is per process,
and a benchmark is one long process, so anything measuring a single command clears
that cache in untimed setup. `acquire_many` keeps it within an iteration, because
there one invocation really does write N events. `roundtrip` clears it twice,
because `acquire` and `release` really are two separate commands.

## Where the time actually goes

The lock file is free and everything around it is not. Resolving the repository
topology — which runs on essentially every command — is **5.6 µs**, and claiming
the lock is a `write` plus a `hard_link`. That is the coordination primitive, and
it is microseconds.

The milliseconds are all subprocesses and log parses. An acquire of an existing
file decomposes roughly as:

| Component | Cost | Why it runs |
|---|---|---|
| `git rev-parse --short HEAD` | ~2.4 ms | `events::append` stamps `head` on hold boundaries |
| `git hash-object -w` | ~2 ms | stamps `content_hash` so [`watch`](watch.md) can diff later |
| one parse of the event log | ~2.6 ms | the prior-claim and stale-copy warnings, now sharing it |
| the lock file, resolution, everything else | ~0.6 ms | the actual claim |

### The subprocess is the single biggest cost, and it is isolated by measurement

`stamp_context` stamps `head` for exactly four kinds — `acquired`, `stolen`,
`released`, `force-released`. Those are the hold boundaries, which is the lease hot
path, and `head_short` shells out to `git rev-parse`.

Two pairs measure it directly. Same append, same log size, differing only in
whether a subprocess runs:

```
events/append/by_kind/notified      397 279 ns   (no head stamped)
events/append/by_kind/acquired    3 098 162 ns   (head stamped, resolved cold)

events/append/head_cache/warm       402 170 ns   (head already resolved)
events/append/head_cache/cold     2 774 912 ns   (resolves it)
```

**A boundary append is ~2.4 ms of subprocess and ~0.4 ms of everything else.** It
also explains the two fastest lease numbers: `refresh_reentrant` writes `renewed`
and the refusal path writes `refused`, neither in the allowlist, so both come in
near 500 µs while doing very similar filesystem work.

### The event log is read whole, and it does not matter

`events::append` finds the previous chain point by reading the entire file, so it
is **O(log size)** in complexity. Measured, at the sizes the cap allows, it is
free:

```
events/append/0      393 027 ns
events/append/1000   395 312 ns
events/append/4500   414 795 ns
```

Flat. The read is sequential, unparsed, and page-cached, and the log is trimmed at
5000 lines. This is worth stating because it was the *first* hypothesis for where
the time went, and it was wrong — the subprocess was hiding it. Reading the tail
instead of the whole file would be correct and would buy nothing, which is why
[it was not done](#what-was-optimised-and-what-was-left-alone).

### Batching now amortises, where it used to be flat

`acquire_many` costs **4.3–4.7 ms per path** at 5 and 20 paths against 8.9 ms at
one, because one invocation resolves HEAD once and reuses it. Before that cache it
was 8.6–8.7 ms per path at every size — every path paid its own subprocess.

Batching also buys [all-or-nothing atomicity](leases.md#claiming-several-paths-at-once),
which is why the protocol asks for it. It now buys speed as well.

### Leasing an existing file is the common case, and it is the cheaper one

`acquire_clean` (8.7 ms, a path that does not exist) is *slower* than
`acquire_hashes_an_existing_file` (6.0 ms), which looks backwards until you see
why: the existing-file path is the one the shared log parse helps, because the
stale-copy check only runs when there is a content hash to compare.

The distinction matters for which number to quote. In the crucible run **56 of 58
acquires were of files that already existed** and 2 were of paths about to be
created, so the budgeted benchmark models the existing-file case.

## Messaging is measured separately, and is not on this path

`pact msg` spawns the Beads CLI, which is tens of milliseconds — a different order
of magnitude, for a different reason, on a path a lease never touches. That
separation is real and it is enforced by construction, not by convention:

- Every benchmark here runs with **no `bd` on `PATH`**. If any lease operation
  reached the backend, these benchmarks would fail rather than slow down.
- `lease acquire` and `lease release` contain no Beads call. The one apparent
  exception, [`watch`'s release-time delivery](watch.md#guarantees-and-what-they-cost),
  is looked up *after* the lock is gone and the event written, and is skipped
  entirely when nothing subscribes to the path.

So the design claim survives, narrowed to what was actually measured: **the Beads
backend is not on the lease hot path. `git` is.**

## The CI budget

One nightly job (`.github/workflows/bench.yml`, plus `workflow_dispatch`) runs
`scripts/bench-budget.sh` and fails if the median of
`lease/roundtrip_acquire_release` exceeds **100 ms**.

**The budget has moved twice, and both times the measurement moved first.** 10 ms
was the original proposal and the baseline was 11.9 ms, so it was rejected before
it ever ran. 50 ms held while the budgeted benchmark leased a path that did not
exist — then field data showed that is the 3% case, the benchmark was corrected to
lease an existing file, and the representative baseline is **20.3 ms** because it
also pays a content hash and a stale-copy check.

100 ms is ~5x that, and the multiple is for the runner rather than the code: the
budgeted path spawns three subprocesses (two `git rev-parse`, one
`git hash-object`), and process spawn is the most runner-sensitive thing in this
whole measurement.

**What it can and cannot catch.** It is an order-of-magnitude contract:

- **Catches**: another subprocess or two on the lease path, a network call, an
  fsync per event, or a walk that grows with history instead of being capped.
- **Does not catch**: a 2x slowdown. Nothing here would notice acquire going from
  8 ms to 16 ms.

That limit is deliberate. Criterion's real strength is comparing a run to a stored
baseline, and this job does not use it, because on a shared runner a baseline
comparison reports the neighbours: the same commit swings wider than any regression
worth finding. An absolute ceiling with large headroom is the assertion that means
something in that environment.

**Not on pull requests, and not a required check** — same reasoning as
[the canary](development.md#canary-pact-against-a-real-beads-cli). It asserts a
property of the machine as much as of the code.

## What was optimised, and what was left alone

The first measurement listed three obvious optimisations and deliberately took
none of them, so that the numbers and the changes could not be confused. Two were
then done and one was rejected **by the measurement itself** (pact-hxy):

**Done — resolve HEAD once per command.** `git_history` memoises `head_short` per
(process, repository). Sound because HEAD cannot move inside a command that exits;
`pact ui` is the one long-lived process and drops the cache on every refresh, so
its staleness is one tick rather than a session.

- `acquire_many/5`: 43.1 ms → 23.6 ms (**−45%**)
- `acquire_many/20`: 174.4 ms → 85.9 ms (**−51%**)
- a single-path command: **unchanged**, and that is not a disappointment but the
  shape of the fix. One boundary event means one resolve either way. The win is on
  the batching the protocol already asks agents to do.

**Done — one log parse instead of two.** `owner_of` and `last_released_content`
were adjacent calls on the acquire path, each parsing the whole log to answer a
question about the same path. `events::acquire_facts` answers both from one pass.

- acquiring an existing file: 8.0 ms → 6.0 ms (**−24%**), which is 56 of 58 real
  acquires
- acquiring a path that does not exist: unchanged, because the second read never
  happened there — the stale-copy check returns before reading when there is no
  content hash to compare

**Rejected — read the log's tail for the chain point.** It would make `append`
O(1) in log size instead of O(n), it is obviously correct, and it buys **nothing**:
append is flat at 393/395/415 µs across 0/1000/4500 lines. The read is sequential,
unparsed and page-cached, and the cap keeps it small. Filed and closed with the
number rather than left as a standing invitation.

### Three measurement errors, all found before publication

Worth recording, because each would have published a figure nobody experiences:

1. **Uncomparable log sizes.** Each benchmark had its own tempdir whose log grew at
   its own rate, so `refresh` looked 6x faster than `acquire` when the real
   difference was events written per iteration. Every repo is now seeded to the
   steady state.
2. **Per-process caching read as per-operation.** With the HEAD cache in, the bench
   resolved once and reused it for every iteration, showing a 51% improvement on a
   single acquire that no user gets. Single-command benchmarks now clear the cache
   in untimed setup.
3. **`BatchSize::SmallInput` batches setups.** It runs every setup, then every
   routine — so clearing the cache N times up front left only the first iteration
   cold. That understated a boundary append by 7x. All reset-dependent benchmarks
   use `PerIteration`.

The general lesson is the one this repo keeps relearning: a measurement that agrees
with your hypothesis deserves more scrutiny than one that does not.

### Still on the table

Nothing urgent. **49 acquire-and-release cycles per second is not anybody's
bottleneck** against a measured p90 lease hold of 24 minutes, so a 20 ms claim is
invisible next to the work it protects. If a future field run shows batched
acquires mattering at fleet scale, the remaining spawn is `git hash-object` on the
acquire path, which could in principle be cached the same way HEAD now is.

## Running it yourself

```bash
mise run bench                      # the budget assertion, which is what CI runs
scripts/bench-budget.sh 25          # a tighter budget, for a quiet machine
cargo bench --bench lease           # the whole suite, full precision (slow)
cargo bench --bench lease -- events # one group
```

`criterion` is a **dev-dependency only**: nothing in `[dependencies]` changes, so
the shipped binary is byte-identical with or without it. The bench reaches pact's
modules through `#[path]` includes rather than a new `[lib]` target — see
`benches/lease.rs`'s header for why, and for what that costs.
