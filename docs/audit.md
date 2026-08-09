---
title: pact audit
description: What each check proves, what `--compare` answers, and what audit deliberately cannot see.
audience: operators
---

# `pact audit`

Offline analysis of a repository's coordination history, from
`.pact/events.jsonl`.

```bash
pact audit                            # summary
pact audit --check double-win         # exit 1 if two agents ever held one path
pact audit --check stale-holds        # exit 1 on holds past TTL with no renew
pact audit --check chain-integrity    # exit 1 if a chain-tracked line was edited
pact audit --check commit-correlation # exit 1 on a real concurrent write or a commit with no lease
pact audit --check topology --expect worktrees   # exit 1 if the fleet did not run where it was told
pact audit --compare base.json        # what moved since a previous --export
pact audit --since 7d --json          # narrow the window, machine-readable
```

Exit **0** clean, **1** findings. Those are the documented codes reused, not
extended: a finding is a *result*, so it is not a usage error (5) and not a lease
conflict (2). `pact audit` with no `--check` always exits 0 — a summary has
nothing to fail.

## Why it exists

pact has always recorded every acquire, renew, release, steal and expiry. Nothing
read it back except `pact log`, which prints the tail. So the questions a fleet
actually raises needed a human with `jq` and a hypothesis:

- is one file a bottleneck for the whole fleet, or merely busy?
- does anybody hold leases for far longer than their peers?
- **did two agents ever hold one path at the same time?**

The last one is different in kind, because it has a written trigger condition
attached. The guard-file backlog item (**pact-ehi**) says: implement the guard
file *if and only if* a double-win appears in a real events log. That is a
falsifiable claim, and until now it had no detector — which makes it a claim
nobody can act on, and an invitation to implement the guard file on suspicion.

`--check double-win` is that detector. The bead names this command; this
command's `--help` names the bead. If it ever exits 1, its output **is** the
evidence the bead is waiting for.

## The checks

### `--check double-win`

Reconstructs hold windows per path and reports any moment two different agents
had one open. `acquired` and `stolen` open a window; `released`,
`force-released` and `expired` close one; `renewed`, `restored` and `refused`
are neither — `restored` is a multi-path `lease acquire` retracting a refresh
it had to undo when a later path in the same batch failed (see
[leases.md](leases.md)), and it un-counts that refresh's `renewed` so a
rolled-back hold can still be flagged by `--check stale-holds`. `refused` is a
denied acquire, logged under the agent who was refused rather than the holder
— it never had a window to open, so it can't skew the holder's own.

Four decisions in that reconstruction are what make it usable rather than noisy,
and each has a test:

| Situation | Verdict | Why |
|---|---|---|
| `acquired` → `released` → `acquired` | clean | sequential holds |
| `expired` → `stolen` | clean | a routine reclaim. `expired` is logged under the **dead** holder's name, so the old window closes before the new one opens |
| `acquired` → `stolen` with no `expired` | **finding** | `--steal` over a live lease really is an overlap |
| `acquired` → `acquired` by the same agent | clean | pact re-acquires to refresh; that is one window, not two |

Get the second row wrong and every takeover in every log is a false finding, at
which point nobody reads the check. Get the third wrong and the one case worth
catching is invisible.

Forensics name both agents, both timestamps, and the **line numbers** in
`events.jsonl`. Events carry no id of their own, and inventing one would mean
rewriting an append-only log whose only virtue is being append-only — "line 47"
is stable for a given file and a human can go and look.

### `--check stale-holds`

Holds that ran past their TTL **and never renewed**, plus any hold that lapsed
into `expired` whatever its length.

**Each hold is judged against the TTL it recorded**, from `ttl_secs` in the
event — never against the TTL the binary happens to be compiled with. That is
what makes the default recalibratable: when
[`DEFAULT_TTL_SECS` moved from 900 to 2700](leases.md#lifecycle-expiry-and-stealing)
on 2026-08-06, no historical finding changed. A hardcoded threshold would have
silently cleared 22 of them, with nothing having changed about the holds.

Events written before pact recorded a TTL fall back to **900s, the default of
their era**, and the report marks those rows `*` and counts them. Judging old
history against today's default is how raising a default quietly rewrites the
past.

The "never renewed" half is the point. The protocol says a long task must not
outlive its lease and that `pact lease renew` refreshes it, so a two-hour hold
that renewed is an agent following instructions. Reporting it would train people
to ignore the check. A two-hour hold that never renewed is a window where the
lease was reclaimable by anyone while its holder still believed it owned the path.

Rows sharing an agent and a duration are one `lease acquire` that named several
paths. The output says so rather than reporting a distinct-incident count,
because grouping them would need a timestamp tolerance — one `acquire` writes one
row per path, microseconds apart — and a number that depends on an arbitrary
tolerance is worse than no number.

### `--check chain-integrity`

Every event pact appends carries `chain_hash`: a hash of the line's own content
mixed with the `chain_hash` of the line physically before it (or a fixed
`"genesis"` value, for the first line ever tracked). Edit a chain-tracked line
after the fact — by hand, or by anything other than `pact` itself — and its
recorded hash no longer matches what this check recomputes from its neighbour.
This is a different failure mode from a torn append (a truncated final line,
already covered by `unparseable_lines`): a torn line fails to *parse* at all,
where a tampered one parses fine and reads as ordinary history unless something
recomputes its hash.

This is **strictly additive, not a change in what pact trusts**. `owner_of`,
`actors`, `audit::summary` and every other existing reader still trust every
line exactly as they did before this field existed — this check is a new,
separate surface, not a new gate in front of an old one. That scope is
deliberate: making the chain the thing those consumers trust would change what
"the log is authoritative" means for all of them, which is a maintainer
decision this feature does not make on anyone's behalf.

A line with **no** `chain_hash` is not a finding. It is counted and reported —
"N line(s) predate chain tracking or were not written by pact" — but never
flagged as tampered, because every line written before this shipped has no
`chain_hash`, including this repository's own committed history. Treating an
absent field as evidence of tampering would flag every pre-existing repository
the moment this check landed. What the check actually flags is narrower and
stronger: a line that DOES carry a `chain_hash`, but the wrong one — which is
exactly the shape a hand-edit or forgery leaves, since nothing except
`pact`'s own append path can compute one that verifies.

Chain continuity resets to `"genesis"` after any untracked line, rather than
reaching back through it for an earlier tracked ancestor. That is what lets a
log that is part pre-existing history and part chain-tracked (the shape every
real repository has, indefinitely, from the moment this shipped) verify
cleanly: a tracked line only ever answers for the line immediately before it.

Alone among the checks, this one reads the **raw, unfiltered** log —
`--since` and `--include-annotated` do not apply to it. The chain is a
property of physical line adjacency as written; an annotation line and
whatever it covers are still real entries the writer's hash chain ran
through, whatever a lease-history statistic later excludes them from.

### `--check commit-correlation`

Every other check looks only at `.pact/events.jsonl` — this one asks
whether real git history backs up what that log claims, by shelling out to
`git log` (`git_history.rs`), the same way `repo.rs` and `doctor.rs` already
shell out to `git` for other checks. It reports three things, only two of
which are findings:

- **A hold with no commit anywhere in its own window** — informational only,
  never a finding. A read-only lease (research, review, a task that turned up
  nothing to change) closes exactly like this, and flagging every one would
  train a reader to ignore the check. `--json`'s `holds_with_no_commit`.
- **A concurrent write** — two holds of the same path with overlapping
  windows where **two or more real commits**, not just the lease events,
  landed during the overlap. Stronger evidence than `--check double-win`,
  which only proves the *lease* events overlapped: this proves work actually
  landed more than once during the disputed period. It does not try to
  attribute which commit belongs to which hold by matching commit author
  against agent name — that correlation is exactly as unreliable as the one
  `pact doctor`'s "Beads actor attribution" check exists to flag (a shared
  checkout collapses every agent's commits to one git identity) — so it
  reports every commit found in the overlap and leaves attribution to a
  human. `--json`'s `concurrent_writes`.
- **An uncovered commit** — a commit touching a path with no hold covering
  its author date at all: work done with no lease, which the whole protocol
  exists to prevent. Scoped to paths that were leased at *some* point in the
  window audited — a path nobody has ever leased is a different question
  ("is this file even under pact's protocol") this check does not try to
  answer, since most of a real repository is never leased at any given
  moment and flagging all of it would be pure noise. `--json`'s
  `uncovered_commits`.

**Degrades cleanly when git can't answer.** A brand-new repository with zero
commits, a `git` that fails to run, or a `.pact/` whose `.git` is not
actually a readable repository all read as "no commit history to correlate
against" rather than crashing or reporting a false blanket set of findings —
`--json`'s `git_unavailable` names the reason when `git` itself could not be
run at all.

Two known gaps, both cheaper to document than to close:

- **Merge commits carry no file list** from a plain `git log --name-only`,
  so a merge that brought in changes to a leased path is invisible here.
  Findings degrade to "no commit seen" — the same shape as a read-only
  lease, never a false positive.
- **A rename is two identities.** No `--follow` is passed, so a lease taken
  under one name only correlates against commits recorded under that same
  name; a renamed file's history before the rename does not connect.

This is the one check where widening scope past `.pact/` was a conscious
call, not an oversight — see
[What audit deliberately cannot see](#what-audit-deliberately-cannot-see)
below for why that does not touch the Beads-store invariant the rest of this
page rests on.

## Where the run actually happened

The summary counts events by the worktree pact was invoked from, so "did this
fleet use the topology I asked for" has an answer:

```
  run in 31 from main, 12 from wt-render
```

Events written before pact recorded this are counted as **predating context
stamping** and never dressed up as a topology — every log that exists today is
in that state, and a check that guessed would be worse than one that abstains.
See [leases.md](leases.md#every-event-records-where-pact-was-invoked-from) for
the fields themselves.

One note fires, and only on unambiguous evidence — the repository has linked
worktrees *right now*, at least one event is context-stamped, and not one names
a worktree:

```
  note   this repository has linked worktrees, but no event was invoked from one
         — agents may be editing in worktrees while running pact from the main
         checkout, in which case the lease/edit binding rests on convention and
         cannot be verified from this log
```

That is the shape a real 20-agent run had: one git worktree per agent, and
every `pact lease` run from the main checkout. It is not a defect — repo-
relative lock keys mean the shared namespace still worked — but it is the
difference between a binding that was verified and one that was assumed.

**Deliberately not inferred from merge commits.** That was the obvious
heuristic and it is a bad one: a repo merging ordinary feature branches has
exactly the same commit shape, so the hint would fire nearly everywhere and
mean nothing — the same failure this page already records under
[Worked example](#worked-example-measuring-the-things-that-motivated-this),
where a metric returned the same answer regardless of the behaviour it claimed
to measure. `has_worktrees` is a fact about the repository, so this cannot
false-positive. Its one blind spot is honest: worktrees deleted after the run
leave nothing to detect.

### `--check topology --expect <worktrees|main|any>`

The summary reports where a run happened; this turns it into an assertion, so
CI can answer "did the fleet use the topology I asked for" with an exit code.

```
$ pact audit --check topology --expect worktrees
topology: scanned 43 event(s)
  expected worktrees; 0 event(s) carry no invocation context

TOPOLOGY MISMATCH: 43 event(s) invoked from "main", which --expect worktrees does not allow
```

**Every context-stamped event must satisfy the expectation. There is no
proportion threshold, and that strictness is the point.** Any looser rule needs
a cutoff — what fraction of events counts as "worktrees"? — and a verdict that
depends on a cutoff nobody derived from data is exactly the failure this page
records under [Worked example](#worked-example-measuring-the-things-that-motivated-this).
All-or-nothing is explainable in one sentence and cannot drift. A genuinely
mixed run therefore satisfies neither `worktrees` nor `main`, which is the
honest answer rather than an inconvenient one.

`outside` never satisfies `--expect worktrees`: it means pact ran somewhere not
under this repository at all, the one value that says the lease/edit binding
cannot be assumed.

`--expect any` (also what a bare `--check topology` means) never fails — it
reports the distribution. That is what `--export` records, so a stored
retrospective does not fail on an expectation nobody declared when it was
written.

**A log with no invocation context exits 0 whatever was expected**, and says how
many events it could not speak for. Every log written before pact 0.7.0 is
entirely in that state; failing them would have failed every repository on the
day this shipped.

## `--compare`

```bash
pact audit --export base.json        # before a change
# … change the protocol, the module layout, the fleet recipe …
pact audit --compare base.json       # what moved
```

```
compared against base.json

FIELD                        BASELINE        NOW      DELTA
events                              2          4         +2
agents                              1          3         +2
refusals (contention)               0          1         +1

15 field(s) unchanged
```

An instrument that reports each run in isolation cannot tell you whether a
change improved anything. Establishing that took three hand-run `jq`
comparisons in a single afternoon of improving pact from field runs, and **two
of the three produced a wrong conclusion** a human had to correct.

**Movement, never a verdict.** Which direction is good depends on what you
changed and why, which the log cannot know. Scoring a run would need weights
nobody derived from data — the failure this page records under
[Worked example](#worked-example-measuring-the-things-that-motivated-this) —
so `--compare` always exits 0 and leaves the judgement where it belongs.

**A protocol shift is called out first**, before any number:

```
PROTOCOL CHANGED between these runs: 6cd5cc61 -> 97b43b5d.
They are not a controlled comparison — anything below may be the
protocol rather than the fleet.
```

That is the exact mistake the feature exists to prevent. 223 messages from
pact's own fleet were once cited as evidence agents message voluntarily; every
one predated the protocol change that suppressed them, and nothing said so.
The era stamp ([onboarding.md](onboarding.md#what-init-writes)) is what makes
this mechanical.

**A field the baseline does not carry is "not comparable", never a delta from
zero.** An older pact's export has no `unacknowledged_messages` at all, and
reporting `0 → 3` would be a fabricated finding. The one exception is a count
in a map that only lists what occurred — `by_kind/refused` is absent from a run
with no contention, and that genuinely is zero.

`--compare` is its own mode and conflicts with `--check`: both want to be the
single thing stdout says, and two JSON values on one stdout breaks every
`--json` caller.

## `--export`

```bash
pact audit --export report.json                      # summary + every check + doctor, as one file
pact audit --check double-win --export report.json    # --export rides along with any --check
pact audit --export report.json --since 7d --json     # combines with every other flag
```

One JSON file bundling `summary`, `double_win`, `stale_holds`,
`chain_integrity`, `commit_correlation`, `topology`, `doctor` (`pact doctor`'s
own checks) and `unacknowledged_messages` — the exact set of things a real field
audit (pact-juz) had to assemble by hand: a separate `pact doctor`, a separate
`pact audit` per named check, and a raw grep of `.pact/events.jsonl`. Meant to
be read directly by a human, or handed to another agent session asking "how is
pact actually being used, and where does it fall down".

`unacknowledged_messages` lists every message its own recipient never marked
read, distinguishing "nobody has read it" from "read only by someone who was
not the addressee" — see
[messaging.md](messaging.md#and-pact-audit---export-asks-it-for-everybody),
which also explains why this cannot live in `pact doctor` (it needs a `bd
list`, which takes a write lock, and doctor is served over MCP as strictly
read-only).

A top-level `observations` array pulls out short, human-readable highlights
— a nonzero finding count from any check, or a `doctor` check that is not a
clean `ok`/no-`warn` — so a reader does not have to re-derive "is this worth
looking at" from raw counts and thresholds. An empty list means nothing rose
to that bar; the structured fields above it are always the complete data
regardless of what lands there.

**Orthogonal to `--check` and `--json`.** `--export` only ever adds a file;
it never changes what prints to stdout or which exit code a `--check` or
plain summary produces — pass it alongside either, or alone. Under `--json`
its own confirmation is skipped rather than printed as a second top-level
object on stdout: every other command's `--json` promises exactly one
parseable value, and a caller piping through `jq` would break on two. In
human mode it prints the path it wrote.

## Closes with nothing open

Reconstruction pairs every `released` / `force-released` / `expired` with the
`acquired` / `stolen` it closes, keyed by agent and path. A close matching no
open hold used to be dropped where it was found — no hold, no counter, no
trace — so `by_kind`'s raw count of close events could disagree with how many
holds actually closed, and nothing said why. The summary and every `--check`
now report it, and `--json` carries it as `orphaned_closes`:

```
  note   8 close event(s) with no matching open — not counted as a Hold
```

It counts "this did not add up" and never guesses what: audit does not
synthesize a best-effort hold for history it cannot reconstruct. A nonzero
count is normal. This repository's own 8 are all explained, and none is a
defect:

- **Four are `force-released`, from before pact-m7j.2.6.** That event is
  logged under the agent *doing* the forcing, unlike `expired`, which is
  deliberately logged under the dead holder — so `reconstruct` looked for an
  open window under the *forcer's* name, found none, and let the real
  holder's window run on until they next touched the path (why a hold
  spanning one used to read as a single long window rather than two). Fixed
  going forward: `force-released` now also carries `displaced`, the holder's
  own name, as a structured field, and `reconstruct` closes that agent's
  window instead. These four predate the field and stay orphaned exactly as
  before — `displaced` is absent on them, not wrong, and nothing rewrites
  history — but a force-release logged from here on closes correctly.
- **Four have an acquire the log never recorded.** `cli-wire` released four
  source files eight minutes into this log's history and no `acquired` for any
  of them appears anywhere in the file: it claimed them before the first line
  was written. A hold that straddles the start of recorded history has a close
  and no open, permanently.

Two more causes look identical to those and are equally benign: a `--since`
window that begins after the open, and an annotation covering an open line but
not its close. What is left after all four is a line the log actually lost — a
torn append, or a trim that raced a writer — which is the only reason the
counter is worth printing.

## Provenance: the log is append-only, corrections are annotations

`.pact/events.jsonl` is committed and never rewritten. Entries are not edited or
deleted, including wrong ones — because the file is *evidence*, and a log somebody
can quietly tidy is not evidence. The guard-file bead (**pact-ehi**) reads it to
decide whether a real defect exists; that only means anything if nobody can remove
an inconvenient line.

So a wrong entry is **annotated**. An `annotation` event names the lines it
covers, says why, and attributes the claim:

```json
{"at":"2026-08-06T…","agent":"maintainer","kind":"annotation",
 "detail":"synthetic: manual expiry experiment, agents victim/ghost/grabber",
 "covers_lines":[40,41,42,43,44,45],"actor":"maintainer"}
```

Rules that follow from that:

- Annotated lines are **excluded from every statistic and every check by
  default**, and the count is always reported — `6 event(s) excluded by
  annotation`. A statistic that quietly omits data is one nobody can check, which
  is the same defect as an editable log.
- `--include-annotated` shows the raw log as written, so an annotation can be
  **disputed** rather than merely trusted. The lines are still there.
- The annotation row itself is never counted as an event, in either mode.
  Otherwise a correction would inflate the totals with a record that describes the
  log rather than the fleet.
- The `actor` is **format-checked and flagged, never enforced**. One that fails
  `identity::validate` (the same `[a-z0-9][a-z0-9-]{1,31}` every agent name
  must match) prints `[INVALID ACTOR — does not match [a-z0-9][a-z0-9-]{1,31}]`
  after the name, and the annotation still takes effect. Rejecting it would let
  one malformed field silently swallow the correction it was written to record,
  which is worse than the forgeable field already was. An absent `actor` prints
  as `unknown` and is *not* flagged — nobody signed this is a different
  condition from somebody signed it with garbage. pact writes no annotation
  itself, so read time is the only gate there is to put this behind.
- Older pact binaries need no change: `kind` is a `String`, so an annotation
  parses as an unknown kind, opens no hold window and closes none. They just do
  not apply the exclusion — which over-reports rather than hiding events, the safe
  direction.

**The `pact-ehi` double-win trigger counts unannotated events only.** An
annotated overlap is not evidence, and there is a test asserting exactly that: the
same two overlapping events fire the check unannotated and do not fire it
annotated.

### The incident (2026-07-31)

Six events in this repository's log are not real coordination history:

| line | agent | kind | path |
|---|---|---|---|
| 40 | `victim` | acquired | `shared.rs` |
| 41 | `grabber` | acquired | `new.rs` |
| 42 | `grabber` | released | `new.rs` |
| 43 | `ghost` | acquired | `ghost.rs` |
| 44 | `ghost` | expired | `ghost.rs` |
| 45 | `victim` | expired | `shared.rs` |

None of those paths has ever existed here. They came from hand-run expiry and
all-or-nothing atomicity experiments executed **from the repository root** on
2026-07-31 — before `PACT_STATE_DIR` existed — and they were committed along with
the log on 2026-08-06 when `.pact/events.jsonl` was first preserved.

Provenance was established by measurement rather than by reading code: the log was
hashed before and after the full test suite and a fleet-sim run, and came back
byte-identical both times. So no test or script writes there today. `lease.rs`,
`cli.rs`, `mcp.rs`, `events_log.rs` and `worktree.rs` all use tempdirs;
`mcp_absent.rs` was the one file spawning pact without a `current_dir`, and though
neither command it runs writes state, it now uses a tempdir plus
`PACT_STATE_DIR` — a test that *cannot* reach real state is worth more than one
that currently happens not to.

Two structural changes came out of it:

- **`PACT_STATE_DIR`** overrides state resolution entirely, so tests, the fleet
  harness and demos can be pointed somewhere harmless. `pact doctor` reports it
  loudly, because a repository with it set by accident is one whose history is
  going somewhere nobody is looking.
- **`scripts/fleet-sim.sh` refuses to start** unless the state directory pact
  resolves is inside its tempdir. Asserted rather than forced: that harness exists
  partly to exercise the real resolution chain, including `--worktrees` and
  `--scope-local`, so it has to let resolution happen and then check the result.

What the correction changed, concretely: 153 events became 147, 23 agents became
20, and **both** `expired` events in the whole history turned out to be
synthetic — so no lease in pact's real history has ever actually lapsed.
`stale-holds` went from 24 findings to 22.

One embarrassment worth recording, since this page is about evidentiary hygiene:
the first annotation appended covered only line 45, because the shell command
building it assigned the line list to `LINES` — a variable bash owns and
overwrites with the terminal height. It happened to be 45. That annotation was
uncommitted and had been read by nothing, so it was replaced rather than
corrected-in-place; the append-only discipline protects recorded history, and a
botched write nobody has seen is not history.

## What audit deliberately cannot see

**The Beads side.** Audit never opens `.beads/`, a Dolt directory, a SQLite
file or a JSONL export. pact's whole messaging design rests on "never touch
the backend store directly, only the CLI", and an analytics command is
exactly where that would be convenient to break, because the data is right
there in a file. Messages, read state, claim discipline and bead provenance
are therefore **not** audit's subject; they live in
[`scripts/beads-retro.sh`](../scripts/beads-retro.sh), which is best-effort,
jq-based, and says so in its own header.

This is a different invariant from `--check commit-correlation` reading git
history directly (above). `git` is a hard requirement of running pact at
all — not a store pact promises to only ever touch through an indirection
layer — so reading its history breaks nothing the Beads rule protects.
`repo.rs` and `doctor.rs` already shell out to `git` directly for other
checks; commit-correlation is the same read, applied to history instead of
working-tree state.

**Anything before the history was preserved.** `.pact/` used to be gitignored
wholesale, so every clone started with an empty log
([architecture.md](architecture.md#pacteventsjsonl-is-committed-and-it-is-not-runtime-state)).
Audit can only see what was kept. A repository that has not re-run `pact init`
since that change still has no history to analyse, and `pact doctor`'s **event
log survives a clone** check is where that gets noticed.

**Conflicts.** The log records what *happened*, not what was refused. An exit-2
conflict writes no event, so audit cannot tell you how often agents blocked each
other. `.pact/waits/` exists for that and is telemetry, not history.

**Anything about intent.** A stale hold might be an agent that crashed or one
doing genuinely long work badly. Audit reports the shape; the reason needs a
human.

## Worked example: measuring the things that motivated this

Three findings from a fleet retrospective were the reason `pact audit` was
written. Two of them did not survive being measured, which is worth more than the
feature.

**"Zero preserved lease history"** — true, and now fixed. `.pact/` was gitignored
wholesale, so nothing could be asked after the fact. Narrowing that rule is
`pact audit`'s prerequisite, not a nice-to-have: the command has nothing to read
without it.

**"Dangling commit hashes, ~30%"** — an artefact of how you count. A naive scan
for 7-40 hex characters over closed beads in this repo gives 17 dangling out of
50, or 34%, which matches. But **9 of those 17 are not commit references at all**:
UUID fragments, `CLAUDE_CODE_SESSION_ID` pieces, a bd version hash (`20e493e56`),
trace ids from a telemetry table. Of the 8 that are genuine, most are deliberate
citations of *another* repository's commits, which cannot resolve here and should
not. Filtering to hashes actually introduced by a provenance word gives **0
dangling out of 15**. The 30% was measuring the regex, not the repository.

**"Claim skip 87%"** — the number was wrong, and so was the correction that
replaced it. This page said for a while that claim adherence is "not measurable
from the available data, in either direction", because bd writes a
`field_change/status` interaction only on close. The status half of that is
true. The conclusion was not.

`bd update --claim` "sets assignee to you, status to `in_progress`" — and bd
logs **assignee** changes as interactions of their own. A self-claim has a
distinctive shape: `actor` equals the new value, with an empty old value. Two
fleet runs carry it plainly:

| run | assignee interactions | status/closed | claim adherence |
|---|---|---|---|
| megablast | 22 | 23 | 22/23 |
| grimcast | **0** | 22 | **0/22** |

So it was measurable all along, through the wrong field — and the third run
regressed to zero claims while closing every bead. The likeliest source of the
original scratch-store result is that `--claim` is documented as **idempotent**:
claiming an issue already assigned to you changes nothing, so it logs nothing.
Testing the no-op path and generalising from it is the mistake.

The lesson this bullet exists for still stands, just aimed one step further
back: a metric that returns the same answer regardless of behaviour is worse
than no metric — and so is concluding a thing cannot be measured because the
first field you looked at did not move.

Whether pact should *report* claim adherence is a separate question and
deliberately still open: this data is Beads-side, and
[audit reads `.pact/` and never the Beads store](#what-audit-deliberately-cannot-see).

The general lesson is the one this repo keeps relearning: a metric that returns
the same answer regardless of the behaviour it claims to measure is worse than no
metric, because it looks like evidence.

## What it says about pact's own history

At the time of writing, on 147 preserved events from 20 agents — six more are in
the file and excluded by annotation, see [Provenance](#provenance-the-log-is-append-only-corrections-are-annotations):

```
$ pact audit --check double-win
double-win: scanned 147 event(s)
  6 event(s) excluded by annotation — this check did not look at them
no overlapping hold windows — no two agents ever held one path at once
```

So **pact-ehi's trigger condition has not fired**. The guard file remains
unjustified, and now that is a measured statement rather than an absence of
complaints.

`--check stale-holds` does find 22 holds past TTL with no renew, the longest
36m6s against the 15m TTL those leases recorded. That is a protocol-adherence
finding about the agents, not a defect in pact: they held paths their leases no
longer covered. It is also the measurement that
[recalibrated the default to 45 minutes](leases.md#lifecycle-expiry-and-stealing) —
and those 22 findings still stand after the bump, because each hold is judged
against its own recorded TTL.

## Where it runs

- **[`scripts/fleet-verify.sh`](../scripts/fleet-verify.sh)** calls
  `--check double-win --json` instead of the 40 lines of jq it used to carry. Two
  implementations of one invariant is one too many, and the jq copy would have had
  to independently know all four rows of the table above.
- **[`.github/workflows/audit.yml`](../.github/workflows/audit.yml)** runs
  `double-win` and `stale-holds` weekly against this repository's own committed
  history, so pact audits its own development continuously and files an issue
  when either turns up something. The three newer checks —
  `chain-integrity`, `commit-correlation` and `topology` — are **not** wired
  into that workflow yet. Said plainly rather than left to be assumed: a
  reader who sees a green weekly audit should know exactly which questions it
  asked.
- **MCP**: `pact_audit_summary` exposes the summary as a sixth read-only tool
  ([mcp.md](mcp.md)). The named checks stay CLI-only, because their contract is an
  exit code and a tool result cannot express one.
