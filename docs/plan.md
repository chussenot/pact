---
title: pact plan lint
description: Check a wave plan before you spawn a fleet — what the manifest is, how to export one, and why this check exists at all.
audience: operators
---

# `pact plan lint`

Run this **before** spawning. Five field runs say planning quality, not lease
arbitration, decides whether a fleet contends.

## Why this exists

A lease resolves a collision *after* two agents have already been sent at one file.
A plan that never sends them there costs nothing to arrange and cannot be contended.

The evidence is unusually one-sided. quern (37 agents) was built to contend — deep
coupling, three declared hot files, one shared spec — and ran its first 249 events
with **zero refusals**, reaching **1 refusal in 64 claims** as the run wound down.
Across five runs (arkanoid, megablast, crucible, grimcast, quern) the only sustained
contention ever measured was the crucible's, and that was engineered on purpose to
see what refusal storms look like. Full numbers in
[studies/field-runs.md](studies/field-runs.md).

The point is not that refusals never happen — one did, and the lease handled it
exactly as designed. It is that their rate is set by the plan, not by the arbitration:
a fleet of 37 agents on a deliberately coupled codebase produced one.

So the contention work happens at planning time. Until now it happened entirely in an
orchestrator's head, with nothing to check it. That is what this lints.

The one rule worth stopping a run for is **intra-wave file overlap** — the megablast
rule, from the run where two agents in one wave were given the same file and
discovered it the hard way.

## The manifest

A manifest is the plan for one run, in a form something other than a human can read.
It is the **contract between your orchestrator and `pact plan lint`** — nothing else
in pact produces or consumes it, and pact never writes one.

### Encoding

Either **a JSON array** of entry objects, or **JSONL**: one entry object per line.

The two are told apart by the first non-whitespace character: a `[` means array,
anything else means JSONL. Both exist because a shell loop naturally emits JSONL and a
script naturally emits an array, and rejecting either would only mean a `jq` incantation
in this page.

In JSONL, **blank lines are skipped** — a shell loop will leave them. In an array,
formatting is irrelevant.

```json
[
  {"id": "parser-ddl", "wave": 1, "files": ["src/parser/ddl.rs"], "depends_on": []},
  {"id": "exec-ddl",   "wave": 2, "files": ["src/exec/ddl.rs"],   "depends_on": ["parser-ddl"]}
]
```

```jsonl
{"id":"parser-ddl","wave":1,"files":["src/parser/ddl.rs"],"depends_on":[]}
{"id":"exec-ddl","wave":2,"files":["src/exec/ddl.rs"],"depends_on":["parser-ddl"]}
```

### The entry

| Field | Type | Required | Default | Meaning |
|-------|------|----------|---------|---------|
| `id` | string | **yes** | — | Names this entry. Opaque to pact. |
| `wave` | integer | no | `null` | Which wave it runs in. |
| `files` | array of strings | no | `[]` | Repo-relative paths it will write. |
| `depends_on` | array of strings | no | `[]` | `id`s that must finish first. |

**`id` is opaque, and deliberately not a bead id.** pact never looks it up anywhere. It
appears only in findings, so it should be something you recognise — a bead id, a slug, a
worker name. It must be unique: every other check keys on it, so a duplicate makes the
whole report ambiguous rather than merely wrong, which is why that is an error.

**`wave` is an ordering key, not a schedule.** Any integer, including negative ones.
Entries sharing a wave are understood to run concurrently, and that is the only thing
pact concludes from it. Absent is a **warning, not an error**: a partly-planned manifest
is the normal intermediate state, and an entry with no wave is simply not checked for
overlap. It cannot be a string — `"wave": "1"` is a parse error naming the line and
column.

**`files` is what the entry will WRITE**, not what it will read. Reads do not contend.
Paths are normalized exactly as [`pact lease acquire`](leases.md) normalizes them, so
`src/a.rs`, `./src/a.rs` and `src/../src/a.rs` are one path here and one lock there —
the check and the lease it protects cannot disagree about what one file is. A path
listed twice in one entry is **counted once** and mentioned as a warning; an entry
cannot contend with itself.

**`depends_on` must name entries in this same manifest.** A dependency pact cannot see
is an error, because the alternative is silently not checking an ordering you asked for.

### Unknown fields are ignored

An entry may carry anything else you like — `owner`, `slug`, `spec`, `estimate` — and
pact will pass over it. That is deliberate: your orchestrator's own bookkeeping and
pact's checks can live in one file, and adding a field to your side never requires a
change on pact's. The corollary is that **a misspelled field is silently ignored**, so
`"file"` instead of `"files"` reads as an entry claiming no files — which is exactly
what the `entry-claims-no-files` warning is for.

### What is deliberately not in the schema

- **No agent, model, prompt or command.** Who runs an entry is your harness's business;
  pact is only asked whether the plan can contend.
- **No status.** A manifest is a plan, not a record of a run. The record is
  `.pact/events.jsonl`, and `pact audit` reads that.
- **No wave scheduling.** You assign waves. pact only checks that the assignment does
  not put two entries on one file.
- **No timestamps.** A manifest describes intent and is worth nothing after the run, so
  keep it as a build artifact rather than committing it.

### Errors in the manifest itself

A line pact cannot parse is an **error naming the line number**, and nothing is linted.
That is the opposite of how pact reads `.beads/interactions.jsonl`, where a malformed
line is skipped — and the difference is the point. That file is somebody else's export
where a partial answer beats none; this is the plan for a fleet you are about to spawn,
so a line pact cannot read is a line you must fix first.

An **empty** manifest is not an error and not "clean" either: it says it is empty, since
an export that found nothing is usually a broken export rather than an empty plan.

## pact does not read your bead store

Since 0.9.0 pact has no runtime backend: `bd` is the agents' task tracker, and pact
reads only its committed `interactions.jsonl` export. Reconstructing a plan from the
bead graph would put a subprocess back on a pact command's path, which is precisely
the dependency that release removed. See
[architecture.md](architecture.md#one-backend-since-079).

So **the orchestrator exports the manifest**. That is not extra bookkeeping: fleets
already write this down. In the quern run, 9 of 24 beads carried a structured
convention in their descriptions —

```
files: src/exec/dml.rs
spec: docs/quern.md
group: exec
```

— and `bd list --json` hands it to you. A pipeline from that convention to a manifest:

```bash
bd list --json | jq -c '
  .[] | { id: .id,
          wave: ( .description | capture("group:\\s*(?<g>\\S+)").g // null ),
          files: [ .description | scan("files:\\s*(\\S+)") | .[0] ],
          depends_on: [] }' > plan.jsonl

pact plan lint plan.jsonl
```

Map your own `group:` names to wave numbers however your run is staged; pact only
cares that entries sharing a wave do not share a file. Keep the manifest as a
build artifact, not a committed file — it is a snapshot of one run's plan.

## What it checks

**Errors** — exit 1, fix before spawning:

| Finding | Why it stops a run |
|---------|--------------------|
| `intra-wave-overlap` | Two entries in one wave claim one path. They will contend, and one of them can move waves for free. |
| `dependency-cycle` | Reported as the cycle itself, not as "a cycle exists" — finding it yourself in a 40-entry plan is barely better than not being told. |
| `unknown-dependency` | An entry depends on an id no entry provides. |
| `dependency-not-earlier` | A dependency scheduled in the same wave or later. The plan says one thing and schedules another. |
| `duplicate-id` | Every other check keys on `id`, so a duplicate makes the whole report ambiguous. |

**Warnings** — exit 0, worth a look:

| Finding | Why it is only a warning |
|---------|--------------------------|
| `entry-claims-no-files` | Nothing can be checked about what it will touch. Legitimate for research or review work. |
| `entry-has-no-wave` | Not checked for overlap. Normal while a plan is still being staged. |
| `duplicate-file-in-entry` | One entry lists a path twice, or two spellings of it. Counted once — usually a copy-paste. |
| `hot-file` | One path in three or more entries. The plan keeps returning to it, and freezing its interface early is usually cheaper than sequencing around it. |

Warnings of the same kind **collapse into a count** past the first few. A manifest
exported before waves were assigned produces one "has no wave" per entry — on a real
43-entry plan that was 43 of 54 warnings and buried the two that were specific. That
is one fact about the manifest, not 43 facts. Errors are never collapsed: each names
a specific pair somebody has to move.

`--json` gives the whole report, with `error: true` on the findings that set the exit
code.

## Deliberately out of scope

- **Reading `.beads`.** See above; it is the charter.
- **Inferring files from prose.** A guess at which files a bead touches would be
  wrong exactly when it mattered, and it would make a clean lint meaningless.
- **Scheduling waves.** This lints a plan. It does not make one — the judgement about
  what can safely run together is the orchestrator's, and it is the part that has
  been working.
