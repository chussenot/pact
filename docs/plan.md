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
coupling, three declared hot files, one shared spec — and produced **zero refusals**.
Across five runs (arkanoid, megablast, crucible, grimcast, quern) the only real
contention ever measured was the crucible's, and that was engineered on purpose to
see what refusal storms look like. Full numbers in
[studies/field-runs.md](studies/field-runs.md).

So the contention work happens at planning time. Until now it happened entirely in an
orchestrator's head, with nothing to check it. That is what this lints.

The one rule worth stopping a run for is **intra-wave file overlap** — the megablast
rule, from the run where two agents in one wave were given the same file and
discovered it the hard way.

## The manifest

One entry per planned unit of work. Either a JSON array or one object per line
(JSONL); both are accepted, because a shell loop produces the second and a script
produces the first.

```json
{"id": "parser-ddl", "wave": 1, "files": ["src/parser/ddl.rs"], "depends_on": []}
{"id": "exec-ddl",   "wave": 2, "files": ["src/exec/ddl.rs"],   "depends_on": ["parser-ddl"]}
```

| Field | Meaning |
|-------|---------|
| `id` | Opaque to pact. It does **not** have to be a bead id, so a plan can be linted before any beads exist. Must be unique. |
| `wave` | Integer. Entries in the same wave run concurrently. Omitting it is a warning, not an error — a partly-planned manifest is the normal intermediate state, and an entry with no wave is simply not checked for overlap. |
| `files` | Repo-relative paths, normalized exactly as `pact lease acquire` normalizes them — so `src/a.rs` and `./src/../src/a.rs` are one path here and one lock there. |
| `depends_on` | Ids of entries that must finish first. |

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

A malformed manifest line is an **error naming the line number**, not a skipped line.
That is the opposite of how pact reads `interactions.jsonl`, and on purpose: that file
is somebody else's export where a partial answer beats none, while this is the plan
for a fleet somebody is about to spawn.
