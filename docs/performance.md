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
lease path is not filesystem-bound either. It is bound by **`git rev-parse`**,
which pact spawns twice per acquire-and-release cycle, plus three full parses of
its own event log.

Everything below comes from `benches/lease.rs` under criterion, against a
tempdir repo with **no `bd` on `PATH`**. Reproduce with `mise run bench`.

## The numbers

Reference hardware: the figures below were taken on one Linux x86-64 developer
machine with an SSD. **Treat the ratios as the result and the absolute values as
that machine's**, which is also why the CI budget carries 4x headroom rather than
tracking these to the percent.

Every repo is seeded to 4500 event-log lines first — between the trim floor
(4000) and cap (5000) that `events.rs` documents — so each benchmark measures a
repository that has been in use, and the numbers are comparable to each other.

| Benchmark | ns/op | ops/sec | What it is |
|---|---|---|---|
| `repo/resolve/plain` | 5 607 | 178 353 | topology resolution, ordinary checkout |
| `repo/resolve/plain_with_worktrees` | 7 793 | 128 313 | …plus the `worktrees/` scan |
| `repo/resolve/linked_worktree_commondir` | 30 722 | 32 550 | …the commondir walk, from a linked worktree |
| `events/append/by_kind/notified` | 443 801 | 2 253 | an append that stamps no `head` |
| `events/append/by_kind/acquired` | 2 948 953 | 339 | the same append, stamping `head` |
| `events/append/0` | 2 492 470 | 401 | append, empty log |
| `events/append/1000` | 2 548 993 | 392 | append, 1000 lines |
| `events/append/4500` | 2 911 045 | 344 | append, steady state |
| `lease/refresh_reentrant` | 520 887 | 1 920 | re-acquire your own live lease |
| `lease/acquire_contended_exit2` | 618 815 | 1 616 | the refusal path an agent hits at exit 2 |
| `lease/release` | 3 587 253 | 279 | release a lease you hold |
| `lease/acquire_clean` | 8 113 720 | 123 | claim a path nobody holds |
| `lease/roundtrip_acquire_release` | **11 889 523** | **84** | **acquire + release — the budgeted number** |
| `lease/acquire_many/1` | 8 196 314 | 122 | batch of 1 |
| `lease/acquire_many/5` | 43 112 677 | 23 | batch of 5 (8.6 ms/path) |
| `lease/acquire_many/20` | 174 429 802 | 6 | batch of 20 (8.7 ms/path) |
| `lease/with_subprocess/acquire_hashes_an_existing_file` | 8 002 637 | 125 | acquire where `git hash-object` also runs |

## Where the time actually goes

The interesting result is not the totals, it is that **the lock file is free and
everything around it is not.**

Resolving the repository topology — the thing that runs on essentially every
command — costs **5.6 µs**. Claiming the lock is a `write` and a `hard_link`.
Those are the coordination primitive, and they are microseconds.

Then a clean acquire costs **8.1 ms**, three orders of magnitude more. It
decomposes almost exactly:

| Component | Cost | Why it runs |
|---|---|---|
| `git rev-parse --short HEAD` | ~2.5 ms | `events::append` stamps `head` on hold boundaries |
| parse the log for `events::owner_of` | ~2.5 ms | warns about an unresolved prior claim |
| parse the log for `last_released_content` | ~2.5 ms | warns you are on a stale copy ([merge divergence](leases.md#a-lease-is-exclusive-in-time-not-across-worktrees)) |
| the lock file, resolution, everything else | ~0.6 ms | the actual claim |

### The subprocess is the single biggest cost, and it was isolated

`events::append` calls `stamp_context`, which stamps `head` for exactly four
kinds — `acquired`, `stolen`, `released`, `force-released`. Those are the hold
boundaries, which is to say the lease hot path. `head_short` shells out to `git
rev-parse`.

The `by_kind` pair measures the same append at the same log size, differing only
in `kind`:

```
events/append/by_kind/notified    443 801 ns   (no head stamped)
events/append/by_kind/acquired  2 948 953 ns   (head stamped)
```

**85% of an append is that one subprocess.** It also explains the two fastest
lease numbers: `refresh_reentrant` writes `renewed` and the refusal path writes
`refused`, neither of which is in the allowlist — so both come in around 500 µs,
16x faster than an acquire, while doing very similar filesystem work.

The gating was a deliberate choice, made for a good reason and documented in
`events.rs`: stamping `head` on every `notified` would have spawned 87 processes
in the run that motivated it. What the measurement adds is that the boundaries it
kept are the hot path, so the cost did not go away — it concentrated.

### The event log is read whole, three times

`events::append` finds the previous chain point by reading the entire file, and
`owner_of` and `last_released_content` each parse the whole log too. So append is
**O(log size)** — bounded, because the log is trimmed at 5000 lines, but the bound
is thousands of lines rather than nothing.

The append curve (0 → 1000 → 4500 lines) is only **+17%**, because the fixed
subprocess cost dwarfs it. That is the honest ordering: the log read is real and
it is the *second* problem.

### Batching is linear, not amortised

`acquire_many` costs 8.6–8.7 ms per path at 1, 5 and 20 paths — flat. Each path
pays its own subprocess and its own log parses. Batching buys
[all-or-nothing atomicity](leases.md#claiming-several-paths-at-once), which is why
the protocol asks for it, but it buys no speed.

### `git hash-object` is *not* an extra cost

Leasing a path that already exists on disk also stamps `content_hash` so
[`pact watch`](watch.md) can diff against it, which runs `git hash-object -w`.
`lease/with_subprocess/acquire_hashes_an_existing_file` measures 8.0 ms against
`acquire_clean`'s 8.1 ms — indistinguishable. Once a command is already paying
for one process spawn, the second is lost in the noise, which is itself an
argument about where the fix would have to go.

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
`lease/roundtrip_acquire_release` exceeds **50 ms**.

**Why 50 and not 10.** 10 ms was the proposed budget, and the measurement rejected
it: the baseline is 11.9 ms. A budget below the baseline is not a strict budget, it
is a broken one. 50 ms is ~4x the measured median, chosen for the *runner* rather
than the code — a shared CI box is routinely 2–3x slower on process spawn and I/O
than a developer machine, and a check that goes red because a neighbour was busy
is a check people learn to re-run.

**What it can and cannot catch.** It is an order-of-magnitude contract:

- **Catches**: a new subprocess or two on the lease path, a network call, an fsync
  per event, or a walk that grows with history instead of being capped.
- **Does not catch**: a 2x slowdown. Nothing here would notice acquire going from
  8 ms to 16 ms.

That limit is deliberate. Criterion's real strength is comparing a run to a stored
baseline, and this job does not use it, because on a shared runner a
baseline comparison reports the neighbours: the same commit swings wider than any
regression worth finding. An absolute ceiling with large headroom is the assertion
that means something in that environment.

**Not on pull requests, and not a required check** — same reasoning as
[the canary](development.md#canary-pact-against-a-real-beads-cli). It asserts a
property of the machine as much as of the code.

## What this measurement suggests, and what it does not do

The obvious optimisations are visible and deliberately **not** taken here — this
was a measurement exercise, and changing the thing you are measuring in the same
pass is how you end up with neither a number nor a fix:

- Cache `head_short` per process. Every command writes 1–2 boundary events, and
  `acquire_many` writes N; one `rev-parse` per *command* would cut the batch case
  by most of its cost.
- Read the log's tail for the chain point rather than the whole file.
- Share one log parse between `owner_of` and `last_released_content`, which run
  back to back on the same acquire and read the same bytes twice.

Whether any of that is worth doing is a separate question with a real answer:
**84 acquire-and-release cycles per second is not currently anybody's
bottleneck.** An agent holds a lease for minutes — the measured p90 hold is 24
minutes — so a 12 ms claim is invisible next to the work it protects. These notes
exist so that the next person who needs the headroom knows exactly where it is,
not to imply it is needed now.

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
