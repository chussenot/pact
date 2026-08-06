# `pact audit`

Offline analysis of a repository's coordination history, from
`.pact/events.jsonl`.

```bash
pact audit                          # summary
pact audit --check double-win       # exit 1 if two agents ever held one path
pact audit --check stale-holds      # exit 1 on holds past TTL with no renew
pact audit --since 7d --json        # narrow the window, machine-readable
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
`force-released` and `expired` close one; `renewed` is neither.

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

Holds that ran past the TTL (900s) **and never renewed**, plus any hold that
lapsed into `expired` whatever its length.

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

**The Beads side.** Audit reads `.pact/` and nothing else — never `.beads/`, a
Dolt directory, a SQLite file or a JSONL export. pact's whole messaging design
rests on "never touch the backend store directly, only the CLI", and an analytics
command is exactly where that would be convenient to break, because the data is
right there in a file. Messages, read state, claim discipline and bead provenance
are therefore **not** audit's subject; they live in
[`scripts/beads-retro.sh`](../scripts/beads-retro.sh), which is best-effort,
jq-based, and says so in its own header.

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

**"Claim skip 87%"** — not measurable from the available data, in either
direction. bd 1.1.2 writes a `field_change/status` interaction **only on close**:
running `bd update --claim`, which sets status to `in_progress`, appends nothing
at all. Verified on a scratch store — the log had 0 lines after a claim and 1
after the close. So "how many beads were closed without being claimed first"
returns 100% for every repository, whatever anybody did. An early cut of
`beads-retro.sh` shipped exactly that number before the check was run.

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
36m6s against a 15m TTL. That is a protocol-adherence finding about the agents,
not a defect in pact: they held paths their leases no longer covered.

## Where it runs

- **[`scripts/fleet-verify.sh`](../scripts/fleet-verify.sh)** calls
  `--check double-win --json` instead of the 40 lines of jq it used to carry. Two
  implementations of one invariant is one too many, and the jq copy would have had
  to independently know all four rows of the table above.
- **[`.github/workflows/audit.yml`](../.github/workflows/audit.yml)** runs both
  checks weekly against this repository's own committed history, so pact audits
  its own development continuously and files an issue when a check turns up
  something.
- **MCP**: `pact_audit_summary` exposes the summary as a sixth read-only tool
  ([mcp.md](mcp.md)). The named checks stay CLI-only, because their contract is an
  exit code and a tool result cannot express one.
